//! The downstream ports of a virtual hub.
//!
//! The state machine follows chapter 11 of the USB 2.0 specification, as far as a hub without
//! physical ports needs it:
//!
//! - A port has no connection to report as long as it is not powered. When it is powered, or
//!   when a device connects to a powered port, the hub reports the connection and its change.
//! - A reset enables a connected port in the speed of the device below and reports the end of the
//!   reset. It takes no time here.
//! - Removing a device clears connection and enable and reports the change of the connection.
//! - A hub whose upstream port is suspended is suspended itself. When its remote wakeup is enabled
//!   and a device comes or goes, it resumes the upstream port, which the hub above reports as the
//!   end of the suspend of that port. A driver that suspended an idle hub relies on this to learn
//!   about a device that is plugged into the hub later.
//!
//! The controller side, which attaches and detaches the devices below the hub, uses a
//! [`HubHandle`]. The control and interrupt endpoints of the hub device use the same state.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError, Weak},
};

use tokio::sync::watch;
use tracing::{debug, warn};

use crate::device::xhci::real_device::Speed;

/// Bits of wPortStatus (USB 2.0 specification, table 11-21).
pub mod port_status {
    pub const CONNECTION: u16 = 1 << 0;
    pub const ENABLE: u16 = 1 << 1;
    pub const SUSPEND: u16 = 1 << 2;
    pub const POWER: u16 = 1 << 8;
    pub const LOW_SPEED: u16 = 1 << 9;
    pub const HIGH_SPEED: u16 = 1 << 10;
}

/// Bits of wPortChange (USB 2.0 specification, table 11-22).
pub mod port_change {
    pub const CONNECTION: u16 = 1 << 0;
    pub const ENABLE: u16 = 1 << 1;
    pub const SUSPEND: u16 = 1 << 2;
    pub const OVER_CURRENT: u16 = 1 << 3;
    pub const RESET: u16 = 1 << 4;
}

/// Port feature selectors (USB 2.0 specification, table 11-17).
pub mod port_feature {
    pub const ENABLE: u16 = 1;
    pub const SUSPEND: u16 = 2;
    pub const RESET: u16 = 4;
    pub const POWER: u16 = 8;
    pub const C_CONNECTION: u16 = 16;
    pub const C_ENABLE: u16 = 17;
    pub const C_SUSPEND: u16 = 18;
    pub const C_OVER_CURRENT: u16 = 19;
    pub const C_RESET: u16 = 20;
}

/// A request that the hub answers with a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall;

#[derive(Debug)]
struct Child {
    speed: Speed,
    /// The state of the child, if it is a hub, which a reset of the port resets and a suspend of
    /// the port suspends.
    hub: Option<HubHandle>,
}

/// What a hub is connected to.
#[derive(Debug, Clone)]
enum Upstream {
    /// A port of a hub above.
    Hub { hub: Weak<HubShared>, port: u8 },
    /// A root port of the controller, which the controller wakes through this function.
    Root(RootWakeup),
}

#[derive(Clone)]
struct RootWakeup(Arc<dyn Fn() + Send + Sync>);

impl fmt::Debug for RootWakeup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RootWakeup")
    }
}

#[derive(Debug, Default)]
struct Port {
    child: Option<Child>,
    powered: bool,
    enabled: bool,
    suspended: bool,
    /// The bits of wPortChange.
    changes: u16,
}

impl Port {
    const fn connected(&self) -> bool {
        self.powered && self.child.is_some()
    }

    const fn is_enabled(&self) -> bool {
        self.connected() && self.enabled
    }

    fn status(&self) -> u16 {
        let mut status = 0;
        if self.powered {
            status |= port_status::POWER;
        }
        if let Some(child) = self.child.as_ref().filter(|_| self.powered) {
            status |= port_status::CONNECTION;
            if child.speed == Speed::Low {
                status |= port_status::LOW_SPEED;
            }
            if self.enabled {
                status |= port_status::ENABLE;
                if child.speed == Speed::High {
                    status |= port_status::HIGH_SPEED;
                }
                if self.suspended {
                    status |= port_status::SUSPEND;
                }
            }
        }
        status
    }

    /// Back to the state after power-on of the hub: no power, so nothing to report. A device below
    /// stays where it is and is found again when the port is powered.
    const fn power_off(&mut self) {
        self.powered = false;
        self.enabled = false;
        self.suspended = false;
    }
}

#[derive(Debug)]
struct HubState {
    /// The bConfigurationValue of the current configuration, 0 if the hub is not configured.
    configuration: u8,
    remote_wakeup: bool,
    /// The upstream port is suspended.
    suspended: bool,
    /// The hub above, if the hub is connected to the port of one.
    upstream: Option<Upstream>,
    ports: Vec<Port>,
}

impl HubState {
    fn port_mut(&mut self, port: u8) -> Option<&mut Port> {
        self.ports.get_mut(usize::from(port).wrapping_sub(1))
    }
}

/// The state of a hub, shared between its device and the controller.
#[derive(Debug)]
pub struct HubShared {
    ports: u8,
    speed: Speed,
    state: Mutex<HubState>,
    /// Signals every change of the state to the status change endpoint.
    changes: watch::Sender<()>,
}

impl HubShared {
    /// A hub with 1 to 15 downstream ports, which is what the route string of the xHCI slot
    /// context can address.
    pub fn new(ports: u8, speed: Speed) -> Self {
        assert!(
            (1..=15).contains(&ports),
            "a hub has 1 to 15 ports, not {ports}"
        );
        Self {
            ports,
            speed,
            state: Mutex::new(HubState {
                configuration: 0,
                remote_wakeup: false,
                suspended: false,
                upstream: None,
                ports: (0..ports).map(|_| Port::default()).collect(),
            }),
            changes: watch::channel(()).0,
        }
    }

    /// Run `f` on the state, which nobody else can access in the meantime.
    fn with_state<R>(&self, f: impl FnOnce(&mut HubState) -> R) -> R {
        // A panic in another thread leaves the state consistent for our purposes.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut state)
    }

    fn notify(&self) {
        self.changes.send_replace(());
    }

    pub const fn ports(&self) -> u8 {
        self.ports
    }

    pub const fn speed(&self) -> Speed {
        self.speed
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    pub fn configuration(&self) -> u8 {
        self.with_state(|state| state.configuration)
    }

    pub fn set_configuration(&self, configuration: u8) {
        self.with_state(|state| state.configuration = configuration);
    }

    pub fn remote_wakeup(&self) -> bool {
        self.with_state(|state| state.remote_wakeup)
    }

    pub fn set_remote_wakeup(&self, enabled: bool) {
        self.with_state(|state| state.remote_wakeup = enabled);
    }

    fn set_suspended(&self, suspended: bool) {
        self.with_state(|state| state.suspended = suspended);
    }

    fn set_upstream(&self, upstream: Option<Upstream>) {
        self.with_state(|state| {
            state.upstream = upstream;
            state.suspended = false;
        });
    }

    /// A suspended hub that may wake the bus resumes its upstream port, after a device came or
    /// went.
    fn wake_upstream(&self) {
        let upstream = self.with_state(|state| {
            if state.suspended && state.remote_wakeup {
                state.suspended = false;
                state.upstream.clone()
            } else {
                None
            }
        });
        match upstream {
            Some(Upstream::Hub { hub, port }) => {
                if let Some(parent) = hub.upgrade() {
                    parent.resume_from_below(port);
                }
            }
            Some(Upstream::Root(RootWakeup(wake))) => wake(),
            None => {}
        }
    }

    /// The hub below on this port resumes: the end of the suspend of the port is a change that the
    /// driver learns about, and a suspended hub passes the wakeup on to the hub above.
    fn resume_from_below(&self, port: u8) {
        let resumed = self.with_state(|state| {
            state.port_mut(port).is_some_and(|port| {
                let was_suspended = port.suspended;
                if was_suspended {
                    port.suspended = false;
                    port.changes |= port_change::SUSPEND;
                }
                was_suspended
            })
        });
        if resumed {
            debug!("the hub below port {port} wakes the bus");
            self.notify();
            self.wake_upstream();
        }
    }

    /// The length in bytes of the status change bitmap: one bit for the hub and one per port.
    pub fn bitmap_length(&self) -> usize {
        usize::from(self.ports) / 8 + 1
    }

    /// The data of the status change endpoint: bit 0 is for the hub and bit N for port N. There is
    /// no data as long as no port has a change to report.
    pub fn status_change_bitmap(&self) -> Option<Vec<u8>> {
        let ports_with_changes: Vec<usize> = self.with_state(|state| {
            (1..=state.ports.len())
                .filter(|&port| state.ports[port - 1].changes != 0)
                .collect()
        });
        if ports_with_changes.is_empty() {
            return None;
        }
        let mut bitmap = vec![0u8; self.bitmap_length()];
        for port in ports_with_changes {
            bitmap[port / 8] |= 1 << (port % 8);
        }
        Some(bitmap)
    }

    /// wPortStatus and wPortChange of a port as the four bytes that GetPortStatus returns.
    pub fn port_status(&self, port: u8) -> Result<[u8; 4], Stall> {
        self.with_state(|state| {
            let port = state.port_mut(port).ok_or(Stall)?;
            let mut data = [0u8; 4];
            data[..2].copy_from_slice(&port.status().to_le_bytes());
            data[2..].copy_from_slice(&port.changes.to_le_bytes());
            Ok(data)
        })
    }

    pub fn set_port_feature(&self, port: u8, feature: u16) -> Result<(), Stall> {
        /// What the change of a port does to the hub below it.
        enum HubBelow {
            Reset(HubHandle),
            Suspend(HubHandle),
        }

        let hub_below = self.with_state(|state| {
            let port = state.port_mut(port).ok_or(Stall)?;
            match feature {
                port_feature::POWER => {
                    if !port.powered {
                        port.powered = true;
                        if port.child.is_some() {
                            port.changes |= port_change::CONNECTION;
                        }
                    }
                    Ok(None)
                }
                port_feature::RESET => {
                    // Nothing happens to a port without a device, like on a real hub.
                    if port.connected() {
                        port.enabled = true;
                        port.suspended = false;
                        port.changes |= port_change::RESET;
                        Ok(port
                            .child
                            .as_ref()
                            .and_then(|child| child.hub.clone())
                            .map(HubBelow::Reset))
                    } else {
                        Ok(None)
                    }
                }
                port_feature::SUSPEND => {
                    // Only a hub below the port can resume it, by remote wakeup. A device that
                    // could not wake the bus, like an external device or one of the host, would
                    // sleep for good, so its port stays active like a root port without a hub
                    // does. The request succeeds, and the status shows the port not suspended.
                    let hub = port
                        .child
                        .as_ref()
                        .filter(|_| port.is_enabled())
                        .and_then(|child| child.hub.clone());
                    if hub.is_some() {
                        port.suspended = true;
                    }
                    Ok(hub.map(HubBelow::Suspend))
                }
                port_feature::ENABLE => {
                    if port.connected() {
                        port.enabled = true;
                    }
                    Ok(None)
                }
                // Includes the test mode and the indicators, which the hub does not have.
                _ => Err(Stall),
            }
        })?;
        self.notify();
        // The hub below has a state of its own.
        match hub_below {
            Some(HubBelow::Reset(hub)) => hub.bus_reset(),
            Some(HubBelow::Suspend(hub)) => hub.shared.set_suspended(true),
            None => {}
        }
        Ok(())
    }

    pub fn clear_port_feature(&self, port: u8, feature: u16) -> Result<(), Stall> {
        let hub_below = self.with_state(|state| {
            let port = state.port_mut(port).ok_or(Stall)?;
            let hub_below = port.child.as_ref().and_then(|child| child.hub.clone());
            let mut hub_below_changes = None;
            match feature {
                port_feature::ENABLE => {
                    // Disabling is not a port error, so it has no change to report.
                    port.enabled = false;
                    port.suspended = false;
                }
                port_feature::SUSPEND => {
                    // The resume signalling is over right away.
                    if port.suspended {
                        port.suspended = false;
                        port.changes |= port_change::SUSPEND;
                        hub_below_changes = Some(false);
                    }
                }
                port_feature::POWER => {
                    port.power_off();
                    // The hub below lost its power.
                    hub_below_changes = Some(true);
                }
                port_feature::C_CONNECTION => port.changes &= !port_change::CONNECTION,
                port_feature::C_ENABLE => port.changes &= !port_change::ENABLE,
                port_feature::C_SUSPEND => port.changes &= !port_change::SUSPEND,
                port_feature::C_OVER_CURRENT => port.changes &= !port_change::OVER_CURRENT,
                port_feature::C_RESET => port.changes &= !port_change::RESET,
                _ => return Err(Stall),
            }
            Ok(hub_below.zip(hub_below_changes))
        })?;
        self.notify();
        // `true`: the hub below lost its power, `false`: it resumes.
        match hub_below {
            Some((hub, true)) => hub.bus_reset(),
            Some((hub, false)) => hub.shared.set_suspended(false),
            None => {}
        }
        Ok(())
    }
}

/// The controller's view of a hub: the devices below it connect to and disconnect from its ports.
#[derive(Debug, Clone)]
pub struct HubHandle {
    shared: Arc<HubShared>,
}

impl HubHandle {
    pub const fn new(shared: Arc<HubShared>) -> Self {
        Self { shared }
    }

    /// The number of downstream ports.
    pub fn ports(&self) -> u8 {
        self.shared.ports()
    }

    /// The speed of the hub, which is the highest speed of the devices below.
    pub fn speed(&self) -> Speed {
        self.shared.speed()
    }

    /// A device connects to a downstream port. `hub` is the state of the device if it is a hub.
    pub fn connect(&self, port: u8, speed: Speed, hub: Option<Self>) {
        if let Some(hub) = &hub {
            hub.shared.set_upstream(Some(Upstream::Hub {
                hub: Arc::downgrade(&self.shared),
                port,
            }));
        }
        let reported = self.shared.with_state(|state| {
            let entry = state.port_mut(port)?;
            entry.child = Some(Child { speed, hub });
            entry.enabled = false;
            entry.suspended = false;
            if entry.powered {
                entry.changes |= port_change::CONNECTION;
            }
            Some(entry.powered)
        });
        match reported {
            Some(powered) => self.device_changed(powered),
            None => warn!("a device connects to port {port}, which the hub does not have"),
        }
    }

    /// The device on a downstream port is removed.
    pub fn disconnect(&self, port: u8) {
        let reported = self.shared.with_state(|state| {
            let entry = state.port_mut(port)?;
            let was_connected = entry.connected();
            let removed = entry.child.take();
            entry.enabled = false;
            entry.suspended = false;
            if was_connected {
                entry.changes |= port_change::CONNECTION;
            }
            Some((was_connected, removed))
        });
        match reported {
            Some((was_connected, removed)) => {
                // A hub that is gone is not connected to this port anymore.
                if let Some(hub) = removed.and_then(|child| child.hub) {
                    hub.shared.set_upstream(None);
                }
                self.device_changed(was_connected);
            }
            None => warn!("a device disconnects from port {port}, which the hub does not have"),
        }
    }

    /// A device came or went. A change that the driver learns about wakes a suspended hub.
    fn device_changed(&self, reported: bool) {
        self.shared.notify();
        if reported {
            self.shared.wake_upstream();
        }
    }

    /// The hub is on a root port of the controller, which it wakes with `wake` when the hub is
    /// suspended and has remote wakeup.
    pub fn connect_to_root_port(&self, wake: impl Fn() + Send + Sync + 'static) {
        self.shared
            .set_upstream(Some(Upstream::Root(RootWakeup(Arc::new(wake)))));
    }

    /// The root port of the hub was suspended or resumed.
    pub fn set_suspended(&self, suspended: bool) {
        self.shared.set_suspended(suspended);
    }

    /// The upstream port of the hub was reset, or the hub lost its power: it starts from scratch,
    /// and so do the hubs below. The devices stay connected, but are only found again when the
    /// driver powers the ports.
    pub fn bus_reset(&self) {
        let hubs_below: Vec<Self> = self.shared.with_state(|state| {
            state.configuration = 0;
            state.remote_wakeup = false;
            state.suspended = false;
            state
                .ports
                .iter_mut()
                .filter_map(|port| {
                    port.power_off();
                    port.changes = 0;
                    port.child.as_ref().and_then(|child| child.hub.clone())
                })
                .collect()
        });
        debug!("hub reset, {} hubs below", hubs_below.len());
        self.shared.notify();
        for hub in hubs_below {
            hub.bus_reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(ports: u8, speed: Speed) -> (Arc<HubShared>, HubHandle) {
        let shared = Arc::new(HubShared::new(ports, speed));
        (shared.clone(), HubHandle::new(shared))
    }

    fn status(shared: &HubShared, port: u8) -> (u16, u16) {
        let data = shared.port_status(port).unwrap();
        (
            u16::from_le_bytes([data[0], data[1]]),
            u16::from_le_bytes([data[2], data[3]]),
        )
    }

    fn power(shared: &HubShared, port: u8) {
        shared.set_port_feature(port, port_feature::POWER).unwrap();
    }

    #[test]
    fn a_new_hub_has_unpowered_ports_and_nothing_to_report() {
        let (shared, _) = hub(4, Speed::High);

        for port in 1..=4 {
            assert_eq!(status(&shared, port), (0, 0));
        }
        assert_eq!(shared.status_change_bitmap(), None);
        assert_eq!(shared.bitmap_length(), 1);
    }

    #[test]
    fn a_device_connecting_to_a_powered_port_is_reported_in_the_bitmap() {
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 3);
        assert_eq!(status(&shared, 3), (port_status::POWER, 0));

        handle.connect(3, Speed::Full, None);

        assert_eq!(
            status(&shared, 3),
            (
                port_status::POWER | port_status::CONNECTION,
                port_change::CONNECTION
            )
        );
        assert_eq!(shared.status_change_bitmap(), Some(vec![1 << 3]));

        shared
            .clear_port_feature(3, port_feature::C_CONNECTION)
            .unwrap();
        assert_eq!(shared.status_change_bitmap(), None);
        assert_eq!(
            status(&shared, 3),
            (port_status::POWER | port_status::CONNECTION, 0)
        );
    }

    #[test]
    fn a_device_below_an_unpowered_port_shows_up_when_the_port_is_powered() {
        let (shared, handle) = hub(4, Speed::High);

        handle.connect(2, Speed::High, None);
        assert_eq!(status(&shared, 2), (0, 0));
        assert_eq!(shared.status_change_bitmap(), None);

        power(&shared, 2);
        assert_eq!(
            status(&shared, 2),
            (
                port_status::POWER | port_status::CONNECTION,
                port_change::CONNECTION
            )
        );
        assert_eq!(shared.status_change_bitmap(), Some(vec![1 << 2]));

        // Powering a powered port again is not a new connection.
        shared
            .clear_port_feature(2, port_feature::C_CONNECTION)
            .unwrap();
        power(&shared, 2);
        assert_eq!(shared.status_change_bitmap(), None);
    }

    #[test]
    fn a_reset_enables_the_port_with_the_speed_of_the_device() {
        let (shared, handle) = hub(4, Speed::High);
        for (port, speed, speed_bits) in [
            (1, Speed::High, port_status::HIGH_SPEED),
            (2, Speed::Full, 0),
            (3, Speed::Low, port_status::LOW_SPEED),
        ] {
            power(&shared, port);
            handle.connect(port, speed, None);
            shared
                .clear_port_feature(port, port_feature::C_CONNECTION)
                .unwrap();

            shared.set_port_feature(port, port_feature::RESET).unwrap();

            assert_eq!(
                status(&shared, port),
                (
                    port_status::POWER | port_status::CONNECTION | port_status::ENABLE | speed_bits,
                    port_change::RESET
                ),
                "speed {speed}"
            );
            assert_eq!(shared.status_change_bitmap(), Some(vec![1 << port]));
            shared
                .clear_port_feature(port, port_feature::C_RESET)
                .unwrap();
            assert_eq!(shared.status_change_bitmap(), None);
        }
    }

    #[test]
    fn a_reset_of_a_port_without_a_device_changes_nothing() {
        let (shared, _) = hub(4, Speed::High);
        power(&shared, 1);

        shared.set_port_feature(1, port_feature::RESET).unwrap();

        assert_eq!(status(&shared, 1), (port_status::POWER, 0));
        assert_eq!(shared.status_change_bitmap(), None);
    }

    #[test]
    fn removing_the_device_clears_connection_and_enable() {
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::High, None);
        shared.set_port_feature(1, port_feature::RESET).unwrap();
        for change in [port_feature::C_CONNECTION, port_feature::C_RESET] {
            shared.clear_port_feature(1, change).unwrap();
        }
        assert_eq!(shared.status_change_bitmap(), None);

        handle.disconnect(1);

        // Only the change of the connection: a removal is not a port error.
        assert_eq!(
            status(&shared, 1),
            (port_status::POWER, port_change::CONNECTION)
        );
        assert_eq!(shared.status_change_bitmap(), Some(vec![1 << 1]));
    }

    #[test]
    fn disabling_a_port_keeps_the_connection() {
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::Full, None);
        shared.set_port_feature(1, port_feature::RESET).unwrap();

        shared.clear_port_feature(1, port_feature::ENABLE).unwrap();

        let (status, _) = status(&shared, 1);
        assert_eq!(status & port_status::ENABLE, 0);
        assert_eq!(status & port_status::CONNECTION, port_status::CONNECTION);
    }

    #[test]
    fn suspend_and_resume_of_an_enabled_port_with_a_hub() {
        let (shared, handle) = hub(4, Speed::High);
        let (_, inner) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::High, Some(inner));

        // A port that is not enabled does not suspend.
        shared.set_port_feature(1, port_feature::SUSPEND).unwrap();
        assert_eq!(status(&shared, 1).0 & port_status::SUSPEND, 0);

        shared.set_port_feature(1, port_feature::RESET).unwrap();
        shared.set_port_feature(1, port_feature::SUSPEND).unwrap();
        assert_eq!(
            status(&shared, 1).0 & port_status::SUSPEND,
            port_status::SUSPEND
        );

        shared.clear_port_feature(1, port_feature::SUSPEND).unwrap();
        let (status, changes) = status(&shared, 1);
        assert_eq!(status & port_status::SUSPEND, 0);
        assert_eq!(changes & port_change::SUSPEND, port_change::SUSPEND);
    }

    #[test]
    fn a_port_with_a_device_that_cannot_wake_the_bus_does_not_suspend() {
        // Only a hub below the port can resume it. A device that would sleep for good, like an
        // external device or one of the host, keeps its port active, and the request succeeds.
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::Full, None);
        shared.set_port_feature(1, port_feature::RESET).unwrap();
        shared.clear_port_feature(1, port_feature::C_RESET).unwrap();

        shared.set_port_feature(1, port_feature::SUSPEND).unwrap();

        let (status, changes) = status(&shared, 1);
        assert_eq!(status & port_status::SUSPEND, 0);
        assert_eq!(status & port_status::ENABLE, port_status::ENABLE);
        assert_eq!(changes & port_change::SUSPEND, 0);
    }

    #[test]
    fn powering_a_port_off_hides_the_device_until_the_next_power_on() {
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::High, None);
        shared.set_port_feature(1, port_feature::RESET).unwrap();
        for change in [port_feature::C_CONNECTION, port_feature::C_RESET] {
            shared.clear_port_feature(1, change).unwrap();
        }

        shared.clear_port_feature(1, port_feature::POWER).unwrap();
        assert_eq!(status(&shared, 1), (0, 0));

        power(&shared, 1);
        assert_eq!(
            status(&shared, 1),
            (
                port_status::POWER | port_status::CONNECTION,
                port_change::CONNECTION
            )
        );
    }

    #[test]
    fn features_the_hub_does_not_have_stall() {
        let (shared, _) = hub(4, Speed::High);
        const PORT_TEST: u16 = 21;
        const PORT_INDICATOR: u16 = 22;

        for feature in [PORT_TEST, PORT_INDICATOR, 0, 3, 9, 23] {
            assert_eq!(shared.set_port_feature(1, feature), Err(Stall), "{feature}");
            assert_eq!(
                shared.clear_port_feature(1, feature),
                Err(Stall),
                "{feature}"
            );
        }
        // The same for a port the hub does not have.
        for port in [0, 5, 255] {
            assert_eq!(shared.port_status(port), Err(Stall));
            assert_eq!(
                shared.set_port_feature(port, port_feature::POWER),
                Err(Stall)
            );
            assert_eq!(
                shared.clear_port_feature(port, port_feature::C_CONNECTION),
                Err(Stall)
            );
        }
    }

    #[test]
    fn the_bitmap_of_a_hub_with_many_ports_has_two_bytes() {
        let (shared, handle) = hub(15, Speed::Full);
        power(&shared, 15);

        handle.connect(15, Speed::Full, None);

        assert_eq!(shared.bitmap_length(), 2);
        assert_eq!(shared.status_change_bitmap(), Some(vec![0, 1 << 7]));
    }

    #[test]
    fn a_bus_reset_resets_the_hub_and_the_hubs_below() {
        let (outer, outer_handle) = hub(4, Speed::High);
        let (inner, inner_handle) = hub(4, Speed::High);
        outer.set_configuration(1);
        power(&outer, 1);
        outer_handle.connect(1, Speed::High, Some(inner_handle.clone()));
        outer.set_port_feature(1, port_feature::RESET).unwrap();
        inner.set_configuration(1);
        power(&inner, 2);
        inner_handle.connect(2, Speed::Full, None);

        outer_handle.bus_reset();

        assert_eq!(outer.configuration(), 0);
        assert_eq!(inner.configuration(), 0);
        assert_eq!(status(&outer, 1), (0, 0));
        assert_eq!(status(&inner, 2), (0, 0));
        assert_eq!(outer.status_change_bitmap(), None);
        // The devices are still there.
        power(&inner, 2);
        assert_eq!(
            status(&inner, 2).0,
            port_status::POWER | port_status::CONNECTION
        );
    }

    #[test]
    fn resetting_a_port_resets_the_hub_below() {
        let (outer, outer_handle) = hub(4, Speed::High);
        let (inner, inner_handle) = hub(4, Speed::High);
        power(&outer, 1);
        outer_handle.connect(1, Speed::High, Some(inner_handle));
        inner.set_configuration(1);

        outer.set_port_feature(1, port_feature::RESET).unwrap();

        assert_eq!(inner.configuration(), 0);
    }

    /// Two hubs, the inner one on port 1 of the outer one, with powered ports and the inner hub
    /// suspended by its driver, which also armed its remote wakeup.
    fn suspended_inner_hub() -> (Arc<HubShared>, HubHandle, Arc<HubShared>, HubHandle) {
        let (outer, outer_handle) = hub(4, Speed::High);
        let (inner, inner_handle) = hub(4, Speed::High);
        power(&outer, 1);
        outer_handle.connect(1, Speed::High, Some(inner_handle.clone()));
        outer.set_port_feature(1, port_feature::RESET).unwrap();
        for change in [port_feature::C_CONNECTION, port_feature::C_RESET] {
            outer.clear_port_feature(1, change).unwrap();
        }
        power(&inner, 2);
        inner.set_remote_wakeup(true);
        outer.set_port_feature(1, port_feature::SUSPEND).unwrap();
        assert_eq!(
            status(&outer, 1).0 & port_status::SUSPEND,
            port_status::SUSPEND
        );
        assert_eq!(outer.status_change_bitmap(), None);
        (outer, outer_handle, inner, inner_handle)
    }

    #[test]
    fn a_device_plugged_into_a_suspended_hub_wakes_it() {
        let (outer, _, inner, inner_handle) = suspended_inner_hub();

        inner_handle.connect(2, Speed::Full, None);

        // The hub above reports the end of the suspend of the port, and the hub below the device.
        let (status, changes) = status(&outer, 1);
        assert_eq!(status & port_status::SUSPEND, 0);
        assert_eq!(changes, port_change::SUSPEND);
        assert_eq!(outer.status_change_bitmap(), Some(vec![1 << 1]));
        assert_eq!(inner.status_change_bitmap(), Some(vec![1 << 2]));

        // It is awake now: another device does not signal again.
        outer
            .clear_port_feature(1, port_feature::C_SUSPEND)
            .unwrap();
        power(&inner, 3);
        inner_handle.connect(3, Speed::Full, None);
        assert_eq!(outer.status_change_bitmap(), None);
    }

    #[test]
    fn a_device_leaving_a_suspended_hub_wakes_it_too() {
        let (outer, _, _, inner_handle) = suspended_inner_hub();
        inner_handle.connect(2, Speed::Full, None);
        outer
            .clear_port_feature(1, port_feature::C_SUSPEND)
            .unwrap();
        outer.set_port_feature(1, port_feature::SUSPEND).unwrap();

        inner_handle.disconnect(2);

        assert_eq!(status(&outer, 1).1, port_change::SUSPEND);
    }

    #[test]
    fn a_hub_does_not_wake_the_bus_without_remote_wakeup_or_when_it_is_not_suspended() {
        let (outer, _, inner, inner_handle) = suspended_inner_hub();
        inner.set_remote_wakeup(false);
        inner_handle.connect(2, Speed::Full, None);
        assert_eq!(outer.status_change_bitmap(), None);
        assert_eq!(
            status(&outer, 1).0 & port_status::SUSPEND,
            port_status::SUSPEND,
            "the port stays suspended"
        );

        // The driver resumes the port, which ends the suspend and tells the hub it is awake.
        outer.clear_port_feature(1, port_feature::SUSPEND).unwrap();
        outer
            .clear_port_feature(1, port_feature::C_SUSPEND)
            .unwrap();
        inner.set_remote_wakeup(true);
        power(&inner, 3);
        inner_handle.connect(3, Speed::Full, None);
        assert_eq!(outer.status_change_bitmap(), None);
    }

    #[test]
    fn a_device_ports_that_is_not_powered_does_not_wake_the_hub() {
        let (outer, _, _, inner_handle) = suspended_inner_hub();

        // Port 4 of the inner hub has no power, so the hub does not see the device.
        inner_handle.connect(4, Speed::Full, None);

        assert_eq!(outer.status_change_bitmap(), None);
    }

    #[test]
    fn the_wakeup_goes_up_through_all_suspended_hubs() {
        let (top, top_handle) = hub(4, Speed::High);
        let (middle, middle_handle) = hub(4, Speed::High);
        let (bottom, bottom_handle) = hub(4, Speed::High);
        top_handle.connect(1, Speed::High, Some(middle_handle));
        middle_handle_connect(&middle, &bottom_handle);
        // The drivers power the ports and reset the hubs, which resets the hubs below, and arm
        // the wakeup afterwards.
        power(&top, 1);
        top.set_port_feature(1, port_feature::RESET).unwrap();
        power(&middle, 1);
        middle.set_port_feature(1, port_feature::RESET).unwrap();
        power(&bottom, 1);
        for hub in [&top, &middle, &bottom] {
            hub.set_remote_wakeup(true);
        }
        top.set_port_feature(1, port_feature::SUSPEND).unwrap();
        middle.set_port_feature(1, port_feature::SUSPEND).unwrap();
        for hub in [&top, &middle] {
            hub.clear_port_feature(1, port_feature::C_CONNECTION)
                .unwrap();
            hub.clear_port_feature(1, port_feature::C_RESET).unwrap();
        }

        bottom_handle.connect(1, Speed::Full, None);

        assert_eq!(status(&middle, 1).1, port_change::SUSPEND);
        assert_eq!(status(&top, 1).1, port_change::SUSPEND);
        assert_eq!(top.status_change_bitmap(), Some(vec![1 << 1]));
    }

    /// Connect `bottom` to port 1 of `middle`, whose handle the test did not keep.
    fn middle_handle_connect(middle: &Arc<HubShared>, bottom: &HubHandle) {
        HubHandle::new(middle.clone()).connect(1, Speed::High, Some(bottom.clone()));
    }

    #[test]
    fn a_hub_that_loses_its_power_or_is_reset_is_not_suspended_anymore() {
        for lose_power in [false, true] {
            let (outer, _, inner, inner_handle) = suspended_inner_hub();
            if lose_power {
                outer.clear_port_feature(1, port_feature::POWER).unwrap();
                power(&outer, 1);
                outer
                    .clear_port_feature(1, port_feature::C_CONNECTION)
                    .unwrap();
            } else {
                outer.set_port_feature(1, port_feature::RESET).unwrap();
                outer.clear_port_feature(1, port_feature::C_RESET).unwrap();
            }
            // The hub below starts from scratch, with its remote wakeup and its power gone.
            assert!(!inner.remote_wakeup());
            inner.set_remote_wakeup(true);
            power(&inner, 1);

            inner_handle.connect(1, Speed::Full, None);

            assert_eq!(
                status(&outer, 1).1 & port_change::SUSPEND,
                0,
                "lose_power={lose_power}"
            );
        }
    }

    #[test]
    fn a_suspended_hub_on_a_root_port_wakes_the_root_port() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (hub, handle) = hub(4, Speed::High);
        let wakeups = Arc::new(AtomicUsize::new(0));
        let counter = wakeups.clone();
        handle.connect_to_root_port(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        power(&hub, 1);
        power(&hub, 2);

        // Awake, or asleep without a wakeup armed: no signal.
        handle.connect(1, Speed::Full, None);
        handle.set_suspended(true);
        handle.disconnect(1);
        assert_eq!(wakeups.load(Ordering::SeqCst), 0);

        hub.set_remote_wakeup(true);
        handle.connect(2, Speed::Full, None);
        assert_eq!(wakeups.load(Ordering::SeqCst), 1);

        // The hub is awake after it signalled.
        handle.disconnect(2);
        assert_eq!(wakeups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_subscriber_learns_about_changes() {
        let (shared, handle) = hub(4, Speed::High);
        let mut changes = shared.subscribe();
        power(&shared, 1);
        changes.changed().await.unwrap();

        handle.connect(1, Speed::Full, None);

        changes.changed().await.unwrap();
    }
}
