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
//!
//! The controller side, which attaches and detaches the devices below the hub, uses a
//! [`HubHandle`]. The control and interrupt endpoints of the hub device use the same state.

use std::sync::{Arc, Mutex, PoisonError};

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
    /// The state of the child, if it is a hub, which a reset of the port resets.
    hub: Option<HubHandle>,
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
    pub fn new(ports: u8, speed: Speed) -> Self {
        Self {
            ports,
            speed,
            state: Mutex::new(HubState {
                configuration: 0,
                remote_wakeup: false,
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
        let reset_child = self.with_state(|state| {
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
                        Ok(port.child.as_ref().and_then(|child| child.hub.clone()))
                    } else {
                        Ok(None)
                    }
                }
                port_feature::SUSPEND => {
                    if port.is_enabled() {
                        port.suspended = true;
                    }
                    Ok(None)
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
        // The reset reaches a hub below, whose state is another one than ours.
        if let Some(hub) = reset_child {
            hub.bus_reset();
        }
        Ok(())
    }

    pub fn clear_port_feature(&self, port: u8, feature: u16) -> Result<(), Stall> {
        self.with_state(|state| {
            let port = state.port_mut(port).ok_or(Stall)?;
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
                    }
                }
                port_feature::POWER => port.power_off(),
                port_feature::C_CONNECTION => port.changes &= !port_change::CONNECTION,
                port_feature::C_ENABLE => port.changes &= !port_change::ENABLE,
                port_feature::C_SUSPEND => port.changes &= !port_change::SUSPEND,
                port_feature::C_OVER_CURRENT => port.changes &= !port_change::OVER_CURRENT,
                port_feature::C_RESET => port.changes &= !port_change::RESET,
                _ => return Err(Stall),
            }
            Ok(())
        })?;
        self.notify();
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
        let known = self.shared.with_state(|state| {
            let Some(entry) = state.port_mut(port) else {
                return false;
            };
            entry.child = Some(Child { speed, hub });
            entry.enabled = false;
            entry.suspended = false;
            if entry.powered {
                entry.changes |= port_change::CONNECTION;
            }
            true
        });
        if known {
            self.shared.notify();
        } else {
            warn!("a device connects to port {port}, which the hub does not have");
        }
    }

    /// The device on a downstream port is removed.
    pub fn disconnect(&self, port: u8) {
        let known = self.shared.with_state(|state| {
            let Some(entry) = state.port_mut(port) else {
                return false;
            };
            let was_connected = entry.connected();
            entry.child = None;
            entry.enabled = false;
            entry.suspended = false;
            if was_connected {
                entry.changes |= port_change::CONNECTION;
            }
            true
        });
        if known {
            self.shared.notify();
        } else {
            warn!("a device disconnects from port {port}, which the hub does not have");
        }
    }

    /// The upstream port of the hub was reset, or the hub lost its power: it starts from scratch,
    /// and so do the hubs below. The devices stay connected, but are only found again when the
    /// driver powers the ports.
    pub fn bus_reset(&self) {
        let hubs_below: Vec<Self> = self.shared.with_state(|state| {
            state.configuration = 0;
            state.remote_wakeup = false;
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
    fn suspend_and_resume_of_an_enabled_port() {
        let (shared, handle) = hub(4, Speed::High);
        power(&shared, 1);
        handle.connect(1, Speed::High, None);

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
