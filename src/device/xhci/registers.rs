use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use tokio::sync::Notify;
use tracing::{debug, info, trace, warn};

use crate::device::{
    pci::constants::xhci::{
        operational::{portsc, usbcmd, usbsts},
        MAX_SLOTS,
    },
    xhci::{interrupter::EventSender, port::UsbVersion, trb::EventTrb},
};

/// A somewhat simple PORTSC register implementation supporting RW1C bits and
/// handling custom port reset logic.
///
/// The PORTSC register requires us to initially set some bits and
/// later react to 1-to-clear writes (RW1C) to get a device to show up.
/// Additionally the PR bit is handled specific to this implementation to avoid
/// an actual reset. Instead we pretend it was successful.
/// We might need further specific logic or access to the bits or state
/// handling later, for now this implementation is enough.
#[derive(Debug)]
pub struct PortscRegister {
    value: AtomicU64,
    event_sender: EventSender,
    usb_version: UsbVersion,
    port_id: u8,
    /// Whether a request for the link state U3 suspends the port. The device on a port does not
    /// notice, so only a device that can wake the port itself, a hub, can be suspended.
    suspend_supported: AtomicBool,
}

/// CSC, PEC, PRC and PLC.
const BITMASK_RW1C: u64 = 0x00660000;

impl PortscRegister {
    pub const fn new(event_sender: EventSender, usb_version: UsbVersion, port_id: u8) -> Self {
        Self {
            value: AtomicU64::new(portsc::PP | portsc::value::PLS_RXDETECT),
            event_sender,
            usb_version,
            port_id,
            suspend_supported: AtomicBool::new(false),
        }
    }

    /// Let the driver suspend the port (PORTSC.PLS = U3) and resume it again.
    pub fn set_suspend_supported(&self, supported: bool) {
        self.suspend_supported.store(supported, Ordering::Relaxed);
    }

    /// Whether the port is in the link state U3 (suspended).
    pub fn is_suspended(&self) -> bool {
        self.usb_version == UsbVersion::USB2 && self.read() & portsc::PLS == portsc::value::PLS_U3
    }

    /// The device on the port signals resume while the port is suspended. The port goes to the
    /// resume state, and the driver learns about it with a port status change event.
    ///
    /// Returns whether the port was suspended.
    pub fn remote_wakeup(&self) -> anyhow::Result<bool> {
        let woke = self
            .value
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |reg| {
                (self.usb_version == UsbVersion::USB2 && reg & portsc::PLS == portsc::value::PLS_U3)
                    .then_some(reg & !portsc::PLS | portsc::value::PLS_RESUME | portsc::PLC)
            })
            .is_ok();
        if woke {
            self.event_sender
                .send(EventTrb::new_port_status_change_event_trb(self.port_id))?;
        }
        Ok(woke)
    }

    /// Write `value` to the register by overwriting the previously stored one.
    pub fn set(&self, value: u64) {
        self.value
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub const fn usb_version(&self) -> UsbVersion {
        self.usb_version
    }

    /// Read the current register value.
    ///
    /// This function should be called when an MMIO read happens.
    pub fn read(&self) -> u64 {
        self.value.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Update the current register value.
    ///
    /// This function should be called when an MMIO write happens.
    /// RW1C bits are updates according to RW1C semantics.
    /// PR bit is handled by a custom logic path.
    /// All other bits are treated as read-only.
    pub fn write(&self, new_value: u64) -> anyhow::Result<()> {
        let requested_link_state =
            (new_value & portsc::LWS != 0).then_some((new_value & portsc::PLS) >> 5);
        if let Some(state) = requested_link_state {
            trace!(
                "port {}: the driver requests link state {state}",
                self.port_id
            );
        }
        let bits_to_clear = new_value & BITMASK_RW1C;
        let port_reset_bit = new_value & portsc::PR != 0;
        let apply = |reg: u64| {
            let mut new_reg = reg & !bits_to_clear;

            if port_reset_bit {
                Self::port_reset(&mut new_reg, self.usb_version);
            } else if let Some(state) = requested_link_state {
                self.request_link_state(&mut new_reg, state);
            }

            new_reg
        };

        match self.value.try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |reg| Some(apply(reg)),
        ) {
            Ok(previous) => {
                // The end of a reset and the end of a resume are changes for the driver.
                let resumed = !port_reset_bit
                    && requested_link_state == Some(0)
                    && self.usb_version == UsbVersion::USB2
                    && matches!(
                        previous & portsc::PLS,
                        portsc::value::PLS_U3 | portsc::value::PLS_RESUME
                    );
                if port_reset_bit || resumed {
                    let event = EventTrb::new_port_status_change_event_trb(self.port_id);
                    self.event_sender.send(event)?;
                }
                Ok(())
            }
            Err(_) => unreachable!("update function never returns None"),
        }
    }

    /// Follow the request of the driver for a link state, as far as the port has one.
    ///
    /// A USB 2 port that supports it goes to U3 on request while it is enabled, and back to U0
    /// via the resume state, with a change of the link state at the end. The request for anything
    /// else, and every request on a USB 3 port, is ignored: the link of a device behind this
    /// controller is up as long as the device is attached.
    fn request_link_state(&self, register: &mut u64, state: u64) {
        let current = *register & portsc::PLS;
        let requested = state << 5;
        let enabled = *register & (portsc::CCS | portsc::PED) == portsc::CCS | portsc::PED;
        let usb2 = self.usb_version == UsbVersion::USB2;
        let next = match requested {
            portsc::value::PLS_U3
                if usb2
                    && enabled
                    && current == portsc::value::PLS_U0
                    && self.suspend_supported.load(Ordering::Relaxed) =>
            {
                Some(portsc::value::PLS_U3)
            }
            portsc::value::PLS_RESUME if usb2 && current == portsc::value::PLS_U3 => {
                Some(portsc::value::PLS_RESUME)
            }
            portsc::value::PLS_U0
                if usb2
                    && (current == portsc::value::PLS_U3
                        || current == portsc::value::PLS_RESUME) =>
            {
                *register |= portsc::PLC;
                Some(portsc::value::PLS_U0)
            }
            _ => None,
        };
        match next {
            Some(link_state) => *register = *register & !portsc::PLS | link_state,
            None => debug!(
                "port {}: ignoring the request for link state {state}",
                self.port_id
            ),
        }
    }

    fn port_reset(register: &mut u64, usb_version: UsbVersion) {
        match usb_version {
            UsbVersion::USB2 => {
                trace!("driver attempted to write portsc::PR on USB 2");
                let portsc_update_mask = portsc::PRC | portsc::PED | portsc::PLS;
                Self::update_with_mask(
                    register,
                    portsc::value::PLS_U0 | portsc::PED | portsc::PRC,
                    portsc_update_mask,
                );
            }
            UsbVersion::USB3 => {
                Self::update_with_mask(register, portsc::PRC, portsc::PRC);
            }
        }
    }

    /// Update the masked bits with the given value.
    ///
    /// This function is absolute and does not respect RW rules imposed for
    /// driver access. It shall only be called as part of internal controller
    /// logic.
    /// Set bits in `value` not set in `mask` are silently dropped.
    const fn update_with_mask(register: &mut u64, value: u64, mask: u64) {
        let register_clear = *register & !mask;
        let value_checked = value & mask;
        *register = value_checked | register_clear;
    }
}

/// Port Power Management Status and Control (chapter 5.4.9)
///
/// Limitations:
/// 1. no separation between RW and RWS
/// 1. no checks for RsvdP at: 17 <= bits <= 31
#[derive(Debug, Default, Clone)]
pub struct PortpmscRegister {
    value: Arc<AtomicU32>,
}
impl PortpmscRegister {
    pub fn read(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, value: u32) {
        self.value.store(value, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct ConfigureRegister {
    value: Arc<AtomicU32>,
}

impl ConfigureRegister {
    pub fn read(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, value: u32) {
        let slots_enabled = (value & 0xff) as u8;
        let value = if u64::from(slots_enabled) > MAX_SLOTS {
            warn!("driver enables {slots_enabled} slots, but there are only {MAX_SLOTS}");
            (value & !0xff) | MAX_SLOTS as u32
        } else {
            value
        };
        self.value.store(value, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }

    pub fn num_slots_enabled(&self) -> u8 {
        (self.read() & 0xff) as u8
    }
}

#[derive(Debug, Default, Clone)]
pub struct DcbaapRegister {
    value: Arc<AtomicU64>,
}

impl DcbaapRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value & !0x3f, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }
}

/// USB Command Register (chapter 5.4.1)
#[derive(Debug, Clone)]
pub struct UsbcmdRegister {
    value: Arc<AtomicU32>,
    reset_notify: Arc<Notify>,
}

impl UsbcmdRegister {
    pub fn new() -> Self {
        Self {
            value: Arc::new(AtomicU32::new(0)),
            reset_notify: Default::default(),
        }
    }

    pub fn read(&self) -> u64 {
        self.value.load(std::sync::atomic::Ordering::Relaxed).into()
    }

    /// A simple write with a very limited list of allowed bits (RsvdP is also not written).
    pub fn write(&self, value: u64) {
        // Currently writable bits, ignoring any other bits and printing a warning.
        const BITMASK_PRESERVED: u64 = usbcmd::RS | usbcmd::INTE | usbcmd::HCRST;
        if value & !BITMASK_PRESERVED != 0 {
            warn!(
                "received at least one bit that is ignored for USBCMD: {}",
                value & !BITMASK_PRESERVED
            );
        }

        // Preserve an ongoing reset until the reset coordinator clears HCRST.
        self.value
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let hcrst = current as u64 & usbcmd::HCRST;
                let value = (value & BITMASK_PRESERVED) | hcrst;
                // SAFETY: USBCMD is defined as 32 bit and the masks used above enforce it.
                Some(value.try_into().unwrap())
            })
            .unwrap();
        if value & usbcmd::HCRST == usbcmd::HCRST {
            info!("Host Controller Reset requested");
            self.reset_notify.notify_waiters();
        }
    }

    pub fn value_reference(&self) -> Arc<AtomicU32> {
        self.value.clone()
    }

    pub async fn hcrst_notification(&self) {
        self.reset_notify.notified().await;
    }

    pub fn clear_hcrst(&self) {
        self.value
            .fetch_and(!(usbcmd::HCRST as u32), Ordering::Relaxed);
    }
}

/// The status bits of USBSTS that the xHC sets for events and the driver clears (RW1C).
#[derive(Debug, Default, Clone)]
pub struct UsbstsEvents {
    value: Arc<AtomicU32>,
}

impl UsbstsEvents {
    const MASK: u64 = usbsts::EINT | usbsts::PCD;

    /// Set status bits. Bits that are no event bits are ignored.
    pub fn set(&self, bits: u64) {
        self.value
            .fetch_or((bits & Self::MASK) as u32, Ordering::Relaxed);
    }

    fn clear(&self, bits: u64) {
        self.value
            .fetch_and(!((bits & Self::MASK) as u32), Ordering::Relaxed);
    }

    fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed).into()
    }

    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }
}

/// USB Status Register (chapter 5.4.2)
#[derive(Debug)]
pub struct UsbstsRegister {
    usbcmd: Arc<AtomicU32>,
    events: UsbstsEvents,
}

impl UsbstsRegister {
    pub const fn new(usbcmd: Arc<AtomicU32>, events: UsbstsEvents) -> Self {
        Self { usbcmd, events }
    }

    pub fn read(&self) -> u64 {
        let is_running = (self.usbcmd.load(Ordering::Relaxed) as u64 & usbcmd::RS) == usbcmd::RS;
        let hch = if is_running { 0 } else { usbsts::HCH };
        hch | self.events.read()
    }

    /// Writing a 1 clears the event bits (RW1C). All other bits are read-only here.
    pub fn write(&self, value: u64) {
        self.events.clear(value);
    }
}

/// Interrupter Management Register (chapter 5.5.2.1)
///
/// IP is set by the xHC when it puts an event on the Event Ring and cleared by software with
/// a write of 1. IE is a plain read/write bit.
#[derive(Debug, Default, Clone)]
pub struct ImanRegister {
    value: Arc<AtomicU32>,
}

impl ImanRegister {
    const IP: u32 = 0x1;
    const IE: u32 = 0x2;

    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed).into()
    }

    pub fn write(&self, new_value: u64) {
        let new_value = new_value as u32;
        let _ = self
            .value
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let mut value = (current & !Self::IE) | (new_value & Self::IE);
                if new_value & Self::IP != 0 {
                    value &= !Self::IP;
                }
                Some(value)
            });
    }

    pub fn set_pending(&self) {
        self.value.fetch_or(Self::IP, Ordering::Relaxed);
    }

    pub fn interrupt_enabled(&self) -> bool {
        self.value.load(Ordering::Relaxed) & Self::IE != 0
    }

    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct GenericRwRegister {
    value: Arc<AtomicU64>,
}

impl GenericRwRegister {
    pub fn new(value: u64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(value)),
        }
    }

    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
    }
}

/// Event Ring Segment Table Size Register (chapter 5.5.2.3.1)
#[derive(Debug, Default, Clone)]
pub struct ErstszRegister {
    value: Arc<AtomicU64>,
}

impl ErstszRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// bit 0 to including 15
    pub fn segment_table_size(&self) -> u16 {
        self.value.load(Ordering::Relaxed) as u16
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct ErstbaRegister {
    value: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl ErstbaRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn base_address(&self) -> u64 {
        self.value.load(Ordering::Relaxed) & !0x3f
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }

    pub async fn write_notification(&self) {
        self.notify.notified().await;
    }
}

/// Event Ring Dequeue Pointer Register (chapter 5.5.2.3.3)
#[derive(Debug, Default, Clone)]
pub struct ErdpRegister {
    value: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl ErdpRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn dequeue_pointer(&self) -> u64 {
        self.value.load(Ordering::Relaxed) & !0xf
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
        // A permit is kept if nobody waits right now, so a waiter cannot miss the write.
        self.notify.notify_one();
    }

    /// Wait for the driver to write the register.
    pub async fn write_notification(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use crate::device::xhci::interrupter::Interrupter;
    use crate::dynamic_bus::DynamicBus;

    use super::*;

    #[tokio::test]
    async fn portsc_read_write() {
        let async_runtime = tokio::runtime::Handle::current();
        let dma_bus = Arc::new(DynamicBus::new());
        let interrupter = Interrupter::new(dma_bus, &async_runtime);
        let reg = PortscRegister::new(interrupter.create_event_sender(), UsbVersion::USB3, 1);

        reg.set(0x00260203);
        assert_eq!(reg.read(), 0x00260203);

        reg.write(0x0).unwrap();
        assert_eq!(
            reg.read(),
            0x00260203,
            "writing 0 should affect neither the read-only nor the RW1C bits."
        );

        reg.write(0x00200000).unwrap();
        assert_eq!(
            reg.read(),
            0x00060203,
            "writing 1 to bit 21 should clear the bit."
        );

        reg.write(0x00040000).unwrap();
        assert_eq!(
            reg.read(),
            0x00020203,
            "writing 1 to bit 18 should clear the bit."
        );

        reg.write(0x00020000).unwrap();
        assert_eq!(
            reg.read(),
            0x00000203,
            "writing 1 to bit 17 should clear the bit."
        );
    }

    /// A USB 2 port with a device behind it that the driver has enabled with a reset.
    fn enabled_usb2_port(
        suspend_supported: bool,
    ) -> (
        PortscRegister,
        crate::device::xhci::interrupter::tests::testutils::MockInterrupter,
    ) {
        let (event_sender, interrupter) =
            crate::device::xhci::interrupter::tests::testutils::MockInterrupter::new();
        let reg = PortscRegister::new(event_sender, UsbVersion::USB2, 5);
        reg.set_suspend_supported(suspend_supported);
        reg.set(portsc::CCS | portsc::PP | (3 << 10) | portsc::value::PLS_POLLING);
        reg.write(portsc::PR).unwrap();
        assert_eq!(reg.read() & portsc::PED, portsc::PED);
        (reg, interrupter)
    }

    fn write_link_state(reg: &PortscRegister, link_state: u64) {
        reg.write(portsc::LWS | link_state).unwrap();
    }

    #[tokio::test]
    async fn a_usb2_port_with_a_hub_suspends_and_resumes_on_request() {
        let (reg, mut interrupter) = enabled_usb2_port(true);
        assert!(interrupter.await_event().await.is_some(), "the reset");
        assert!(!reg.is_suspended());

        write_link_state(&reg, portsc::value::PLS_U3);
        assert!(reg.is_suspended());
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_U3);
        assert_eq!(reg.read() & portsc::PED, portsc::PED, "still enabled");
        assert!(
            interrupter.is_empty(),
            "the driver caused the change itself"
        );

        // The resume signalling and its end, after which the driver is told once.
        write_link_state(&reg, portsc::value::PLS_RESUME);
        assert!(!reg.is_suspended());
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_RESUME);
        assert!(interrupter.is_empty());
        write_link_state(&reg, portsc::value::PLS_U0);
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_U0);
        assert_eq!(reg.read() & portsc::PLC, portsc::PLC);
        assert!(interrupter.await_event().await.is_some());

        // The driver clears the change.
        reg.write(portsc::PLC).unwrap();
        assert_eq!(reg.read() & portsc::PLC, 0);
        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn the_driver_can_resume_straight_to_u0() {
        let (reg, mut interrupter) = enabled_usb2_port(true);
        interrupter.await_event().await;
        write_link_state(&reg, portsc::value::PLS_U3);

        write_link_state(&reg, portsc::value::PLS_U0);

        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_U0);
        assert!(interrupter.await_event().await.is_some());
    }

    #[tokio::test]
    async fn a_port_without_a_hub_ignores_the_request_for_u3() {
        let (reg, mut interrupter) = enabled_usb2_port(false);
        interrupter.await_event().await;

        write_link_state(&reg, portsc::value::PLS_U3);

        assert!(!reg.is_suspended());
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_U0);
    }

    #[tokio::test]
    async fn a_port_that_is_not_enabled_does_not_suspend() {
        let (reg, mut interrupter) = enabled_usb2_port(true);
        interrupter.await_event().await;
        reg.set(portsc::CCS | portsc::PP | portsc::value::PLS_POLLING);

        write_link_state(&reg, portsc::value::PLS_U3);

        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_POLLING);
    }

    #[tokio::test]
    async fn a_usb3_port_ignores_requests_for_a_link_state() {
        let (event_sender, interrupter) =
            crate::device::xhci::interrupter::tests::testutils::MockInterrupter::new();
        let reg = PortscRegister::new(event_sender, UsbVersion::USB3, 1);
        reg.set_suspend_supported(true);
        reg.set(portsc::CCS | portsc::PED | portsc::PP);

        write_link_state(&reg, portsc::value::PLS_U3);

        assert!(!reg.is_suspended());
        assert_eq!(reg.read(), portsc::CCS | portsc::PED | portsc::PP);
        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn a_device_that_wakes_a_suspended_port_puts_it_in_the_resume_state() {
        let (reg, mut interrupter) = enabled_usb2_port(true);
        interrupter.await_event().await;

        // An awake port has nothing to wake.
        assert!(!reg.remote_wakeup().unwrap());
        assert!(interrupter.is_empty());

        write_link_state(&reg, portsc::value::PLS_U3);
        assert!(reg.remote_wakeup().unwrap());
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_RESUME);
        assert_eq!(reg.read() & portsc::PLC, portsc::PLC);
        assert!(interrupter.await_event().await.is_some());
        assert!(
            !reg.remote_wakeup().unwrap(),
            "the port is not suspended anymore"
        );

        // The driver ends the resume.
        reg.write(portsc::PLC).unwrap();
        write_link_state(&reg, portsc::value::PLS_U0);
        assert_eq!(reg.read() & portsc::PLS, portsc::value::PLS_U0);
    }

    #[test]
    fn usbcmd_read_write() {
        let reg = UsbcmdRegister::new();

        reg.write(0x5);
        assert_eq!(reg.read(), 0x5, "Writing to allowed bits should work.");

        reg.write(0xd);
        assert_eq!(
            reg.read(),
            0x5,
            "Writing any bit besides the allowed ones should be ignored."
        );

        reg.write(0x1);
        assert_eq!(
            reg.read(),
            0x1,
            "Not writing to allowed and already set bit 2 should clear it."
        );
    }

    #[test]
    fn usbcmd_preserves_hcrst_until_reset_completes() {
        let reg = UsbcmdRegister::new();

        reg.write(usbcmd::RS | usbcmd::HCRST);
        assert_eq!(reg.read(), usbcmd::RS | usbcmd::HCRST);

        reg.write(usbcmd::INTE);
        assert_eq!(
            reg.read(),
            usbcmd::INTE | usbcmd::HCRST,
            "HCRST should stay set until the reset coordinator clears it."
        );

        reg.clear_hcrst();
        assert_eq!(
            reg.read(),
            usbcmd::INTE,
            "clearing HCRST should preserve the other writable bits."
        );
    }

    #[test]
    fn usbsts_read_is_consistent_with_usbcmd_status() {
        let usbcmd = UsbcmdRegister::new();
        let usbsts = UsbstsRegister::new(usbcmd.value_reference(), UsbstsEvents::default());

        assert_eq!(
            usbsts.read() & usbsts::HCH,
            usbsts::HCH,
            "HCHalted bit is a 1 whenever the Run/Stop (R/S) bit is a 0."
        );

        usbcmd.write(usbcmd::RS);

        assert_eq!(
            usbsts.read() & usbsts::HCH,
            0,
            "HCHalted bit is a 0 whenever the Run/Stop (R/S) bit is a 1."
        );

        assert_eq!(
            usbsts.read() & (usbsts::EINT | usbsts::PCD),
            0,
            "After creating this register no event happened."
        );
    }

    #[test]
    fn usbsts_event_bits_are_set_by_the_xhc_and_cleared_by_writing_one() {
        let events = UsbstsEvents::default();
        let usbsts = UsbstsRegister::new(UsbcmdRegister::new().value_reference(), events.clone());

        events.set(usbsts::EINT | usbsts::PCD);
        assert_eq!(usbsts.read() & 0x18, 0x18);

        usbsts.write(0);
        assert_eq!(usbsts.read() & 0x18, 0x18, "zeros do not clear");

        usbsts.write(usbsts::EINT);
        assert_eq!(usbsts.read() & 0x18, usbsts::PCD);

        usbsts.write(!0);
        assert_eq!(usbsts.read() & 0x18, 0);
        assert_eq!(
            usbsts.read() & usbsts::HCH,
            usbsts::HCH,
            "the halted bit is not writable"
        );

        events.set(!usbsts::PCD);
        assert_eq!(
            usbsts.read() & 0x18,
            usbsts::EINT,
            "only event bits can be set"
        );
    }

    #[test]
    fn iman_pending_bit_is_cleared_by_writing_one_and_enable_bit_is_plain() {
        let iman = ImanRegister::default();
        assert!(!iman.interrupt_enabled());

        iman.write(0x2);
        assert!(iman.interrupt_enabled());
        assert_eq!(iman.read(), 0x2);

        iman.set_pending();
        assert_eq!(iman.read(), 0x3);

        // Writing the enable bit alone leaves the pending bit alone.
        iman.write(0x2);
        assert_eq!(iman.read(), 0x3);

        // Writing 1 to IP clears it and 0 to IE disables.
        iman.write(0x1);
        assert_eq!(iman.read(), 0x0);
        assert!(!iman.interrupt_enabled());

        iman.set_pending();
        iman.reset();
        assert_eq!(iman.read(), 0x0);
    }

    #[test]
    fn configure_register_limits_the_enabled_slots() {
        let config = ConfigureRegister::default();

        config.write(MAX_SLOTS as u32);
        assert_eq!(config.num_slots_enabled(), MAX_SLOTS as u8);

        config.write(0xff);
        assert_eq!(config.num_slots_enabled(), MAX_SLOTS as u8);
    }

    #[test]
    fn write_erstsz_register_and_read_segment_table_size_field() {
        let erstsz = ErstszRegister::default();
        assert_eq!(erstsz.read(), 0);
        erstsz.write(!0);
        assert_eq!(erstsz.read(), !0);
        assert_eq!(erstsz.segment_table_size(), 0xffff);
    }

    #[test]
    fn write_erstba_register_and_read_base_address_register_field() {
        let erstba = ErstbaRegister::default();
        assert_eq!(erstba.read(), 0);
        erstba.write(!0);
        assert_eq!(erstba.read(), !0);
        assert_eq!(erstba.base_address(), !0x3f);
    }

    #[test]
    fn write_erdp_register_and_read_dequeue_pointer_field() {
        let erdp = ErdpRegister::default();
        assert_eq!(erdp.read(), 0);
        erdp.write(!0);
        assert_eq!(erdp.read(), !0);
        assert_eq!(erdp.dequeue_pointer(), !0xf);
    }
}
