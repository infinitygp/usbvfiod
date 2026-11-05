//! Emulation of a USB3 Host (XHCI) controller.
//!
//! See XHCI specification Section 3 for an overview about the scope of the xHC.

use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tokio::runtime;
use tracing::{debug, error, warn};

use crate::device::{
    bus::{BusDeviceRef, SingleThreadedBusDevice},
    interrupt_line::InterruptLine,
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{offset, operational::usbcmd, MAX_INTRS, RUN_BASE},
        traits::PciDevice,
    },
    xhci::{
        controller_reset::ResetCoordinator,
        endpoint_launcher::EndpointLauncher,
        port::{get_portli_id, get_portpmsc_id, get_portsc_id, HotplugControl, PortArray},
        real_device::CompleteRealDevice,
        registers::{UsbcmdRegister, UsbstsRegister},
        slot_manager::SlotManager,
    },
};
use crate::device::{pci::constants::xhci::capability, xhci::command_ring::CommandRing};
use crate::device::{pci::constants::xhci::OP_BASE, xhci::interrupter::Interrupter};

/// How long we wait for a Host Controller Reset to finish.
const RESET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct XhciController<CRD: CompleteRealDevice> {
    /// The PCI Configuration Space of the controller.
    ///
    /// Only the vfio-user thread accesses this field, so we use a standard Mutex
    /// instead of the tokio variant.
    config_space: Mutex<ConfigSpace>,
    interrupter: Interrupter,
    port_array: PortArray<CRD>,
    command_ring: CommandRing,
    slot_manager: SlotManager,
    usbcmd: UsbcmdRegister,
    usbsts: UsbstsRegister,
}

impl<CRD: CompleteRealDevice> XhciController<CRD> {
    pub fn new(dma_bus: BusDeviceRef, async_runtime: runtime::Handle) -> Self {
        let usbcmd = UsbcmdRegister::new();
        let interrupter = Interrupter::new(dma_bus.clone(), &async_runtime);
        let usbsts = UsbstsRegister::new(usbcmd.value_reference(), interrupter.usbsts_events());
        let port_array = PortArray::new(interrupter.create_event_sender(), async_runtime.clone());
        let ep_launch_requester = EndpointLauncher::start(
            port_array.create_device_retriever(),
            async_runtime.clone(),
            dma_bus.clone(),
            interrupter.create_event_sender(),
        );
        let slot_manager = SlotManager::new(dma_bus.clone(), &async_runtime, ep_launch_requester);
        let command_ring = CommandRing::new(
            dma_bus.clone(),
            &async_runtime,
            interrupter.create_event_sender(),
            slot_manager.create_slot_worker_handle(),
            usbcmd.value_reference(),
        );
        ResetCoordinator::start(
            usbcmd.clone(),
            [
                Box::new(command_ring.reset_sender()),
                Box::new(interrupter.reset_sender()),
                Box::new(slot_manager.reset_sender()),
                Box::new(port_array.reset_sender()),
            ],
            &async_runtime,
        );

        Self {
            config_space: Mutex::new(Self::build_config_space()),
            interrupter,
            port_array,
            command_ring,
            slot_manager,
            usbcmd,
            usbsts,
        }
    }

    fn build_config_space() -> ConfigSpace {
        use crate::device::pci::constants::config_space::*;

        ConfigSpaceBuilder::new(vendor::REDHAT, device::REDHAT_XHCI)
            .class(class::SERIAL, subclass::SERIAL_USB, progif::USB_XHCI)
            // TODO Should be a 64-bit BAR.
            .mem32_nonprefetchable_bar(0, 4 * 0x1000)
            .mem32_nonprefetchable_bar(3, 2 * 0x1000)
            .msix_capability(MAX_INTRS.try_into().unwrap(), 3, 0, 3, 0x1000)
            .config_space()
    }

    /// Reset the controller the way a client resets a PCI function.
    ///
    /// The PCI Configuration Space returns to its initial state and the xHC goes through a Host
    /// Controller Reset. The attached devices stay where they are.
    pub fn reset(&self) {
        *self.config_space.lock().unwrap() = Self::build_config_space();

        self.usbcmd.write(usbcmd::HCRST);

        let deadline = Instant::now() + RESET_TIMEOUT;
        while self.usbcmd.read() & usbcmd::HCRST != 0 {
            if Instant::now() >= deadline {
                error!("Host Controller Reset did not complete");
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn connect_irq(&self, irq: Arc<dyn InterruptLine>) {
        self.interrupter
            .set_interrupt_line(irq)
            .expect("Interrupter should be alive");
    }

    pub fn hotplug_control(&self) -> HotplugControl<CRD> {
        self.port_array.create_hotplug_control()
    }
}

/// Report that a worker of the controller stopped.
///
/// The guest must not be able to take the whole process down with it, and a stopped worker only
/// affects the part of the controller that it implements.
fn log_dead_worker(result: anyhow::Result<()>) {
    if let Err(err) = result {
        error!("a worker of the controller is gone: {err}");
    }
}

/// We do not support 64-bit addressing, so the upper half of an address register is zero.
fn ignore_upper_address_half(register: &str, value: u64) {
    if value != 0 {
        warn!("ignoring {value:#x} written to the upper half of {register}, there is no support for addresses above 4G");
    }
}

impl<CRD: CompleteRealDevice> PciDevice for XhciController<CRD> {
    fn write_cfg(&self, req: crate::device::bus::Request, value: u64) {
        self.config_space.lock().unwrap().write(req, value);
    }

    fn read_cfg(&self, req: crate::device::bus::Request) -> u64 {
        self.config_space.lock().unwrap().read(req)
    }

    fn bar(&self, bar_no: u8) -> Option<super::config_space::BarInfo> {
        self.config_space.lock().unwrap().bar(bar_no)
    }

    fn write_io(&self, region: u32, req: crate::device::bus::Request, value: u64) {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Operational Registers
            offset::USBCMD => self.usbcmd.write(value),
            offset::DNCTL => {
                if value != 2 {
                    debug!("ignoring write of {value:#x} to DNCTL, device notifications are not supported");
                }
            }
            offset::CRCR => log_dead_worker(self.command_ring.control(value)),
            offset::CRCR_HI => ignore_upper_address_half("CRCR", value),
            offset::DCBAAP => self.slot_manager.dcbaap.write(value),
            offset::DCBAAP_HI => ignore_upper_address_half("DCBAAP", value),
            offset::CONFIG => self.slot_manager.config_reg.write(value as u32), // guard.enable_slots(value),
            offset::USBSTS => self.usbsts.write(value),
            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.registers.interrupt_management.write(value),
            offset::IMOD => self
                .interrupter
                .registers
                .interrupt_moderation_interval
                .write(value),
            offset::ERSTSZ => {
                self.interrupter.registers.erst_size.write(value);
            }
            offset::ERSTBA => self.interrupter.registers.erst_base_address.write(value),
            offset::ERSTBA_HI => ignore_upper_address_half("ERSTBA", value),
            offset::ERDP => self
                .interrupter
                .registers
                .eventring_dequeue_pointer
                .write(value),
            offset::ERDP_HI => ignore_upper_address_half("ERDP", value),
            offset::DOORBELL_CONTROLLER => log_dead_worker(self.command_ring.doorbell()),
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => {
                let slot_id = ((req.addr - offset::DOORBELL_CONTROLLER) / 4) as u8;
                log_dead_worker(self.slot_manager.doorbell(slot_id, value as u8));
            }
            addr if get_portsc_id(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_id = get_portsc_id(addr).unwrap();
                log_dead_worker(self.port_array.write_portsc(port_id, value));
            }

            addr if get_portpmsc_id(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_id = get_portpmsc_id(addr).unwrap();
                // SAFETY: The PORTPMSC is defined as 32 bit
                self.port_array.write_portpmsc(port_id, value as u32);
            }

            addr => {
                debug!("ignoring write of {value:#x} to unknown register {addr:#x}");
            }
        }
    }

    fn read_io(&self, region: u32, req: crate::device::bus::Request) -> u64 {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Capability Registers
            // CAPLENGTH and HCIVERSION share a dword; Linux reads both with a single 32-bit access.
            offset::CAPLENGTH => OP_BASE | (capability::HCIVERSION << 16),
            offset::HCIVERSION => capability::HCIVERSION,
            offset::HCSPARAMS1 => capability::HCSPARAMS1,
            offset::HCSPARAMS2 => capability::HCSPARAMS2,
            offset::HCSPARAMS3 => 0,
            offset::HCCPARAMS1 => capability::HCCPARAMS1,
            offset::DBOFF => offset::DOORBELL_CONTROLLER,
            offset::RTSOFF => RUN_BASE,
            offset::HCCPARAMS2 => 0,

            // xHC Extended Capability ("Supported Protocols Capability")
            offset::SUPPORTED_PROTOCOLS => capability::supported_protocols::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_STRING => capability::USB_STRING,
            offset::SUPPORTED_PROTOCOLS_CONFIG => capability::supported_protocols::CONFIG,
            offset::SUPPORTED_PROTOCOLS_CONFIG_RESERVED => 0,
            offset::SUPPORTED_PROTOCOLS_USB2 => capability::supported_protocols_usb2::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_USB2_STRING => capability::USB_STRING,
            offset::SUPPORTED_PROTOCOLS_USB2_CONFIG => capability::supported_protocols_usb2::CONFIG,
            offset::SUPPORTED_PROTOCOLS_USB2_CONFIG_RESERVED => 0,

            // xHC Operational Registers
            offset::USBCMD => self.usbcmd.read(),
            offset::USBSTS => self.usbsts.read(),
            offset::DNCTL => 2,
            offset::CRCR => self.command_ring.status(),
            offset::CRCR_HI => 0,
            offset::DCBAAP => self.slot_manager.dcbaap.read(),
            offset::DCBAAP_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => self.slot_manager.config_reg.read() as u64,

            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.registers.interrupt_management.read(),
            offset::IMOD => self
                .interrupter
                .registers
                .interrupt_moderation_interval
                .read(),
            offset::ERSTSZ => self.interrupter.registers.erst_size.read(),
            offset::ERSTBA => self.interrupter.registers.erst_base_address.read(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.interrupter.registers.eventring_dequeue_pointer.read(),
            offset::ERDP_HI => 0,
            offset::DOORBELL_CONTROLLER => 0, // kernel reads the doorbell after write
            // Device Doorbell Registers (DOORBELL_DEVICE)
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => 0,
            offset::MFINDEX => 0x0,

            // Port Status and Control Register (PORTSC)
            addr if get_portsc_id(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_id = get_portsc_id(addr).unwrap();
                self.port_array.read_portsc(port_id)
            }

            // Port Power Management Status and Control (PORTPMSC)
            addr if get_portpmsc_id(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_id = get_portpmsc_id(addr).unwrap();
                self.port_array.read_portpmsc(port_id) as u64
            }

            // Port Link Info Register (PORTLI_USB3)
            addr if get_portli_id(addr).is_some() => 0,

            // Everything else is Reserved Zero
            addr => {
                debug!("read of unknown register {addr:#x}");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tokio::time::sleep;
    use usbvfiod::hotplug_protocol::response::Response;

    use crate::{
        device::{
            bus::{testutils::TestBusDevice, Request, RequestSize},
            pci::constants::xhci::{
                operational::{crcr, portsc, usbsts},
                rings::trb_types,
            },
            xhci::{
                real_device::{tests::testutils::MockRealDevice, CompleteRealDeviceImpl},
                trb::testutils::RawTrbBuilder,
            },
        },
        dynamic_bus::DynamicBus,
    };

    use super::*;

    type TestController = XhciController<CompleteRealDeviceImpl<MockRealDevice, (u8, u8)>>;

    const DCBAA: u64 = 0x1000;
    const COMMAND_RING: u64 = 0x2000;
    const SEGMENT_TABLE: u64 = 0x3000;
    const EVENT_RING: u64 = 0x4000;
    const EVENT_RING_TRBS: u32 = 16;

    #[derive(Debug, Default)]
    struct CountingInterruptLine {
        count: AtomicUsize,
    }

    impl InterruptLine for CountingInterruptLine {
        fn interrupt(&self) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Plays the part of the driver in front of the controller.
    struct Driver {
        controller: TestController,
        ram: Arc<TestBusDevice>,
        interrupts: Arc<CountingInterruptLine>,
        command_cycle: bool,
        next_command: u64,
        next_event: u64,
        event_cycle: bool,
    }

    impl Driver {
        async fn new() -> Self {
            let ram = Arc::new(TestBusDevice::new(&[0u8; 0x8000]));
            let dma_bus = Arc::new(DynamicBus::new());
            dma_bus.add(0, ram.clone()).unwrap();

            let controller = TestController::new(dma_bus, runtime::Handle::current());
            let interrupts = Arc::new(CountingInterruptLine::default());
            controller.connect_irq(interrupts.clone());
            // Let the workers start, so that they wait for the registers to be written.
            sleep(Duration::from_millis(50)).await;

            let driver = Self {
                controller,
                ram,
                interrupts,
                command_cycle: true,
                next_command: COMMAND_RING,
                next_event: EVENT_RING,
                event_cycle: true,
            };
            driver.start().await;
            driver
        }

        fn write32(&self, addr: u64, value: u32) {
            self.controller
                .write_io(0, Request::new(addr, RequestSize::Size4), value.into());
        }

        fn read32(&self, addr: u64) -> u32 {
            self.controller
                .read_io(0, Request::new(addr, RequestSize::Size4)) as u32
        }

        /// The initialization sequence of a driver.
        async fn start(&self) {
            self.write32(offset::USBCMD, usbcmd::HCRST as u32);
            while self.read32(offset::USBCMD) & usbcmd::HCRST as u32 != 0 {
                sleep(Duration::from_millis(1)).await;
            }

            self.write32(offset::CONFIG, 1);
            self.write32(offset::DCBAAP, DCBAA as u32);
            self.write32(offset::CRCR, COMMAND_RING as u32 | crcr::RCS as u32);

            self.ram
                .write_bulk(SEGMENT_TABLE, &EVENT_RING.to_le_bytes());
            self.ram
                .write_bulk(SEGMENT_TABLE + 8, &EVENT_RING_TRBS.to_le_bytes());
            self.write32(offset::ERSTSZ, 1);
            self.write32(offset::ERDP, EVENT_RING as u32);
            self.write32(offset::ERSTBA, SEGMENT_TABLE as u32);
            // The worker sets up the event ring after the write.
            sleep(Duration::from_millis(50)).await;

            self.write32(offset::IMAN, 0x2);
            self.write32(offset::USBCMD, (usbcmd::RS | usbcmd::INTE) as u32);
        }

        fn ring_command(&mut self, trb_type: u8) -> u64 {
            let address = self.next_command;
            let mut command = RawTrbBuilder::new(address).with_trb_type(trb_type);
            if self.command_cycle {
                command = command.with_cycle();
            }
            let command = command.build();
            self.ram.write_bulk(address, &command.buffer);
            self.next_command += 16;

            self.write32(offset::DOORBELL_CONTROLLER, 0);
            address
        }

        /// Wait for the next event and consume it like a driver does.
        async fn next_event(&mut self) -> [u8; 16] {
            for _ in 0..200 {
                let mut trb = [0u8; 16];
                self.ram.read_bulk(self.next_event, &mut trb);
                if (trb[12] & 1 != 0) == self.event_cycle {
                    self.next_event += 16;
                    // Tell the controller that we are done with the event.
                    self.write32(offset::ERDP, self.next_event as u32 | 0x8);
                    return trb;
                }
                sleep(Duration::from_millis(10)).await;
            }
            panic!("no event arrived");
        }

        fn interrupt_count(&self) -> usize {
            self.interrupts.count.load(Ordering::SeqCst)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn command_completion_raises_the_interrupt_and_the_status_bits() {
        let mut driver = Driver::new().await;
        assert_eq!(driver.read32(offset::USBSTS) & (usbsts::HCH as u32), 0);
        assert_eq!(driver.read32(offset::USBSTS) & (usbsts::EINT as u32), 0);
        assert_eq!(driver.read32(offset::IMAN), 0x2);

        let command = driver.ring_command(trb_types::NO_OP_COMMAND);

        let event = driver.next_event().await;
        assert_eq!(event[13] >> 2, trb_types::COMMAND_COMPLETION_EVENT);
        assert_eq!(event[0..8], command.to_le_bytes());
        assert_eq!(event[11], 1, "completion code is success");

        assert_eq!(driver.interrupt_count(), 1);
        assert_eq!(driver.read32(offset::IMAN), 0x3, "interrupt pending");
        assert_eq!(
            driver.read32(offset::USBSTS) & (usbsts::EINT | usbsts::PCD) as u32,
            usbsts::EINT as u32
        );

        // The driver acknowledges the interrupt.
        driver.write32(offset::USBSTS, usbsts::EINT as u32);
        driver.write32(offset::IMAN, 0x3);
        assert_eq!(driver.read32(offset::IMAN), 0x2);
        assert_eq!(driver.read32(offset::USBSTS) & usbsts::EINT as u32, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_interrupt_while_the_driver_disabled_it() {
        let mut driver = Driver::new().await;
        driver.write32(offset::IMAN, 0x0);

        driver.ring_command(trb_types::NO_OP_COMMAND);
        let event = driver.next_event().await;

        assert_eq!(event[13] >> 2, trb_types::COMMAND_COMPLETION_EVENT);
        assert_eq!(driver.interrupt_count(), 0);
        assert_eq!(
            driver.read32(offset::IMAN),
            0x1,
            "the event is still pending"
        );
        assert_eq!(
            driver.read32(offset::USBSTS) & usbsts::EINT as u32,
            usbsts::EINT as u32
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attaching_a_device_reports_a_port_change() {
        let mut driver = Driver::new().await;
        assert_eq!(driver.read32(offset::USBSTS) & usbsts::PCD as u32, 0);

        let device = CompleteRealDeviceImpl::new((1, 1), MockRealDevice::default());
        let response = driver.controller.hotplug_control().attach(device).await;
        assert_eq!(response, Response::SuccessfulOperation);

        let event = driver.next_event().await;
        assert_eq!(event[13] >> 2, trb_types::PORT_STATUS_CHANGE_EVENT);
        assert_eq!(
            event[3], 1,
            "the super speed device is on the first USB 3 port"
        );
        assert_eq!(
            driver.read32(offset::USBSTS) & usbsts::PCD as u32,
            usbsts::PCD as u32
        );
        assert_eq!(driver.interrupt_count(), 1);

        let portsc = driver.read32(offset::PORTSC);
        assert_eq!(portsc & portsc::CCS as u32, portsc::CCS as u32);
        assert_eq!(portsc & portsc::CSC as u32, portsc::CSC as u32);

        // Writing 1 clears the change bit.
        driver.write32(offset::PORTSC, portsc::CSC as u32);
        assert_eq!(driver.read32(offset::PORTSC) & portsc::CSC as u32, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn port_change_before_the_event_ring_exists_is_still_reported_in_usbsts() {
        let ram = Arc::new(TestBusDevice::new(&[0u8; 0x1000]));
        let dma_bus = Arc::new(DynamicBus::new());
        dma_bus.add(0, ram).unwrap();
        let controller = TestController::new(dma_bus, runtime::Handle::current());
        sleep(Duration::from_millis(50)).await;

        let device = CompleteRealDeviceImpl::new((1, 1), MockRealDevice::default());
        controller.hotplug_control().attach(device).await;
        sleep(Duration::from_millis(50)).await;

        let usbsts = controller.read_io(0, Request::new(offset::USBSTS, RequestSize::Size4));
        assert_eq!(usbsts & (usbsts::PCD | usbsts::EINT), usbsts::PCD);
        let portsc = controller.read_io(0, Request::new(offset::PORTSC, RequestSize::Size4));
        assert_eq!(portsc & portsc::CCS, portsc::CCS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_a_reset_a_connected_device_is_new_and_the_driver_is_told_about_the_change() {
        let mut driver = Driver::new().await;
        let device = CompleteRealDeviceImpl::new((1, 1), MockRealDevice::default());
        driver.controller.hotplug_control().attach(device).await;
        driver.next_event().await;

        // The driver enumerated the device and acknowledged everything.
        driver.write32(
            offset::PORTSC,
            (portsc::CSC | portsc::PEC | portsc::PRC) as u32,
        );
        driver.write32(offset::USBSTS, (usbsts::EINT | usbsts::PCD) as u32);
        assert_eq!(driver.read32(offset::USBSTS) & usbsts::PCD as u32, 0);
        assert_eq!(driver.read32(offset::PORTSC) & portsc::CSC as u32, 0);

        // The device is still connected when the guest comes back from a reset.
        driver.controller.reset();
        sleep(Duration::from_millis(50)).await;

        let portsc = driver.read32(offset::PORTSC);
        assert_eq!(portsc & portsc::CCS as u32, portsc::CCS as u32);
        assert_eq!(portsc & portsc::CSC as u32, portsc::CSC as u32);
        assert_eq!(
            driver.read32(offset::USBSTS) & usbsts::PCD as u32,
            usbsts::PCD as u32
        );
        assert_eq!(
            driver.read32(offset::USBSTS) & usbsts::EINT as u32,
            0,
            "no event went onto a ring"
        );
    }
}
