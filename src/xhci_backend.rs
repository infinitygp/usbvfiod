use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Result};
use nusb::MaybeFuture;
use tokio::runtime;
use tracing::{debug, info, trace, warn};

use usbvfiod::hotplug_protocol::{device_paths::resolve_path, response::Response};
use vfio_bindings::bindings::vfio::{
    vfio_region_info, VFIO_IRQ_SET_ACTION_TRIGGER, VFIO_IRQ_SET_DATA_EVENTFD,
    VFIO_IRQ_SET_DATA_NONE, VFIO_PCI_BAR0_REGION_INDEX, VFIO_PCI_BAR1_REGION_INDEX,
    VFIO_PCI_BAR2_REGION_INDEX, VFIO_PCI_BAR3_REGION_INDEX, VFIO_PCI_BAR4_REGION_INDEX,
    VFIO_PCI_BAR5_REGION_INDEX, VFIO_PCI_CONFIG_REGION_INDEX, VFIO_PCI_MSIX_IRQ_INDEX,
    VFIO_PCI_NUM_IRQS, VFIO_PCI_NUM_REGIONS, VFIO_REGION_INFO_FLAG_READ,
    VFIO_REGION_INFO_FLAG_WRITE,
};
use vfio_user::{IrqInfo, ServerBackend, ServerRegion};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{constants::config_space, traits::PciDevice, xhci::XhciController},
    xhci::{
        nusb::NusbRealDevice,
        port::HotplugControl,
        real_device::{CompleteRealDevice, CompleteRealDeviceImpl},
    },
};

use crate::{
    dynamic_bus::DynamicBus,
    memory_segment::{MemorySegment, UnbackedSegment},
};

#[derive(Debug)]
pub struct XhciBackend<CRD: CompleteRealDevice> {
    dma_bus: Arc<DynamicBus>,
    controller: XhciController<CRD>,
}

#[derive(Debug)]
struct InterruptEventFd {
    /// TODO: Get rid of the Mutex. Writes to the EventFd are safe.
    ///  This just satisfies the Send + Sync requirements and provides
    ///  interior mutability for the [`InterruptLine`] trait.
    fd: Mutex<File>,
}

impl InterruptLine for InterruptEventFd {
    fn interrupt(&self) {
        // Ensure all guest memory modifications can be observed by another process.
        // The sequentially consistent fence emits a hardware memory barrier operation.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

        // Write any 8 byte value to the EventFd.
        // TODO: we just expect this to always work currently.
        let _amount = self
            .fd
            .lock()
            .unwrap()
            .write(&1u64.to_le_bytes())
            .expect("should always be able to write event fd");
    }
}

impl<CRD: CompleteRealDevice> XhciBackend<CRD> {
    /// Create a new virtual XHCI controller with the given USB
    /// devices attached at creation time.
    pub fn new(async_runtime: runtime::Handle) -> Result<Self> {
        let dma_bus = Arc::new(DynamicBus::new());
        let controller = XhciController::new(dma_bus.clone(), async_runtime);

        let backend = Self {
            controller,
            dma_bus,
        };

        Ok(backend)
    }

    pub fn hotplug_control(&self) -> HotplugControl<CRD> {
        self.controller.hotplug_control()
    }

    /// Forget everything the client told us.
    ///
    /// This is what remains to do after the connection to the client closed: Guest memory and
    /// interrupts are gone, and the controller must start from scratch for the next client.
    pub fn disconnect(&self) {
        self.dma_bus.clear();
        self.controller
            .connect_irq(Arc::new(DummyInterruptLine::default()));
        self.controller.reset();
    }
}

impl XhciBackend<CompleteRealDeviceImpl<NusbRealDevice, (u8, u8)>> {
    pub async fn add_device_from_path(
        &self,
        path: impl AsRef<Path>,
        async_runtime: runtime::Handle,
    ) -> Result<()> {
        let (bus, dev, path) = resolve_path(path)?;
        let open_file = |err_msg| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("{}: {}", err_msg, path.display()))
        };

        let file = open_file("Failed to open USB device file")?;
        let device = nusb::Device::from_fd(file.into()).wait()?;
        device.reset().wait()?;

        // After the reset, the device instance is no longer usable and we need
        // to reopen.
        let file = open_file("Failed to open USB device file after device reset")?;
        let device = nusb::Device::from_fd(file.into()).wait()?;
        let real_device = NusbRealDevice::try_new(device, async_runtime.clone())?;
        let complete_device = CompleteRealDeviceImpl::new((bus, dev), real_device);
        let response = self.hotplug_control().attach(complete_device).await;
        if response != Response::SuccessfulOperation {
            return Err(anyhow!(
                "initial attach of device {bus:03}:{dev:03} failed: {response:?}"
            ));
        }

        Ok(())
    }
}

impl<CRD: CompleteRealDevice> XhciBackend<CRD> {
    /// Return a list of regions for [`vfio_user::Server::new`].
    pub fn regions(&self) -> Vec<ServerRegion> {
        (0..VFIO_PCI_NUM_REGIONS)
            .map(|i| {
                let empty_region = ServerRegion {
                    region_info: vfio_region_info {
                        argsz: size_of::<vfio_region_info>() as u32,
                        index: i,
                        ..Default::default()
                    },
                    sparse_areas: vec![],
                    mmap_fd: None,
                };

                match i {
                    VFIO_PCI_CONFIG_REGION_INDEX => {
                        debug!("Client queried config space region");

                        ServerRegion {
                            region_info: vfio_region_info {
                                argsz: size_of::<vfio_region_info>() as u32,
                                index: i,
                                size: 256,
                                flags: VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE,
                                ..Default::default()
                            },
                            sparse_areas: vec![],
                            mmap_fd: None,
                        }
                    }

                    VFIO_PCI_BAR0_REGION_INDEX
                    | VFIO_PCI_BAR1_REGION_INDEX
                    | VFIO_PCI_BAR2_REGION_INDEX
                    | VFIO_PCI_BAR3_REGION_INDEX
                    | VFIO_PCI_BAR4_REGION_INDEX
                    | VFIO_PCI_BAR5_REGION_INDEX => {
                        let bar_no = i - VFIO_PCI_BAR0_REGION_INDEX;

                        u8::try_from(bar_no)
                            .ok()
                            .and_then(|bar_no| self.controller.bar(bar_no))
                            .map_or_else(
                                || {
                                    debug!("Client queried BAR{bar_no} region: (empty)");
                                    empty_region
                                },
                                |bar_info| {
                                    debug!("Client queried BAR{bar_no} region: {:?}", bar_info);
                                    ServerRegion {
                                        region_info: vfio_region_info {
                                            argsz: size_of::<vfio_region_info>() as u32,
                                            index: i,
                                            size: bar_info.size.into(),
                                            flags: VFIO_REGION_INFO_FLAG_READ
                                                | VFIO_REGION_INFO_FLAG_WRITE,
                                            ..Default::default()
                                        },
                                        sparse_areas: vec![],
                                        mmap_fd: None,
                                    }
                                },
                            )
                    }

                    unknown => {
                        debug!("Client queried unknown VFIO region: {unknown}");
                        empty_region
                    }
                }
            })
            .collect()
    }

    /// Return a list of IRQs for [`vfio_user::Server::new`].
    pub fn irqs(&self) -> Vec<IrqInfo> {
        (0..VFIO_PCI_NUM_IRQS)
            .map(|index| IrqInfo {
                index,
                count: match index {
                    VFIO_PCI_MSIX_IRQ_INDEX => 1,
                    _ => 0,
                },
                flags: 0,
            })
            .collect()
    }
}

/// The size of the PCI Configuration Space region we report.
const CONFIG_REGION_SIZE: u64 = config_space::SIZE as u64;

/// The widest access we hand to the controller.
const MAX_ACCESS_SIZE: usize = 4;

impl<CRD: CompleteRealDevice> XhciBackend<CRD> {
    /// Read from a region, splitting what the controller cannot serve in one piece.
    ///
    /// Anything we cannot serve reads as all bits set, like a PCI access that nobody answers.
    fn read_region(&self, region: u32, offset: u64, data: &mut [u8]) {
        data.fill(0xff);

        let region_size = match region {
            VFIO_PCI_CONFIG_REGION_INDEX => CONFIG_REGION_SIZE,
            VFIO_PCI_BAR0_REGION_INDEX => self.bar_size(0),
            _ => 0,
        };
        let Ok(len) = u64::try_from(data.len()) else {
            return;
        };
        if offset.checked_add(len).is_none_or(|end| end > region_size) {
            debug!("read of region {region} offset {offset:#x}+{len} is out of bounds");
            if region == VFIO_PCI_BAR3_REGION_INDEX {
                // The pending bit array of MSI-X is in this region. We never keep an interrupt pending.
                data.fill(0);
            }
            return;
        }

        // The registers of the xHC are 32 bits wide, and 64-bit registers consist of two of them.
        // Splitting an 8-byte access is what the specification allows for a controller without
        // 64-bit addressing.
        for (index, chunk) in data.chunks_mut(MAX_ACCESS_SIZE).enumerate() {
            let Ok(size) = RequestSize::try_from(chunk.len()) else {
                debug!(
                    "read of region {region} with unsupported size {}",
                    chunk.len()
                );
                return;
            };
            let req = Request::new(offset + (index * MAX_ACCESS_SIZE) as u64, size);
            let value = match region {
                VFIO_PCI_CONFIG_REGION_INDEX => self.controller.read_cfg(req),
                _ => self.controller.read_io(0, req),
            };
            chunk.copy_from_slice(&value.to_le_bytes()[0..chunk.len()]);
        }
    }

    fn write_region(&self, region: u32, offset: u64, data: &[u8]) {
        let region_size = match region {
            VFIO_PCI_CONFIG_REGION_INDEX => CONFIG_REGION_SIZE,
            VFIO_PCI_BAR0_REGION_INDEX => self.bar_size(0),
            _ => 0,
        };
        let Ok(len) = u64::try_from(data.len()) else {
            return;
        };
        if offset.checked_add(len).is_none_or(|end| end > region_size) {
            debug!("write to region {region} offset {offset:#x}+{len} is out of bounds");
            return;
        }

        for (index, chunk) in data.chunks(MAX_ACCESS_SIZE).enumerate() {
            let Ok(size) = RequestSize::try_from(chunk.len()) else {
                debug!(
                    "write to region {region} with unsupported size {}",
                    chunk.len()
                );
                return;
            };
            let req = Request::new(offset + (index * MAX_ACCESS_SIZE) as u64, size);
            let mut value = [0u8; 8];
            value[0..chunk.len()].copy_from_slice(chunk);
            let value = u64::from_le_bytes(value);
            match region {
                VFIO_PCI_CONFIG_REGION_INDEX => self.controller.write_cfg(req, value),
                _ => self.controller.write_io(0, req, value),
            }
        }
    }

    fn bar_size(&self, bar_no: u8) -> u64 {
        self.controller
            .bar(bar_no)
            .map_or(0, |bar_info| bar_info.size.into())
    }
}

impl<CRD: CompleteRealDevice> ServerBackend for XhciBackend<CRD> {
    fn region_read(
        &mut self,
        region: u32,
        offset: u64,
        data: &mut [u8],
    ) -> Result<(), std::io::Error> {
        self.read_region(region, offset, data);

        trace!(
            "read region {region} offset {offset:#x}+{} val {:?}",
            data.len(),
            data
        );

        Ok(())
    }

    fn region_write(
        &mut self,
        region: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        trace!(
            "write region {region} offset {offset:#x}+{} val {:?}",
            data.len(),
            data
        );

        self.write_region(region, offset, data);

        Ok(())
    }

    fn dma_map(
        &mut self,
        flags: vfio_user::DmaMapFlags,
        offset: u64,
        address: u64,
        size: u64,
        fd: Option<File>,
    ) -> Result<(), std::io::Error> {
        info!("dma_map flags = {flags:?} offset = {offset} address = {address:#x} size = {size:#x} fd = {fd:?}");

        let segment: BusDeviceRef = match fd {
            Some(fd) => {
                let access_rights = flags.try_into().map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("DMA map at {address:#x}: {err}"),
                    )
                })?;
                Arc::new(MemorySegment::new_from_fd(
                    &fd,
                    offset,
                    size,
                    access_rights,
                )?)
            }
            None => {
                // Guest memory like ROMs and device memory the VMM does not share with other
                // processes. A driver cannot use it as DMA buffer, as long as it comes from RAM that
                // is shared.
                warn!(
                    "DMA mapping at {address:#x}+{size:#x} comes without a file descriptor and is not accessible"
                );
                Arc::new(UnbackedSegment::new(address, size))
            }
        };

        self.dma_bus.add(address, segment).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("DMA map at {address:#x}+{size:#x}: {err}"),
            )
        })
    }

    fn dma_unmap(
        &mut self,
        flags: vfio_user::DmaUnmapFlags,
        address: u64,
        size: u64,
    ) -> Result<(), std::io::Error> {
        info!("dma_unmap flags = {flags:?} address = {address:#x} size = {size:#x}");

        if flags.contains(vfio_user::DmaUnmapFlags::UNMAP_ALL) {
            self.dma_bus.clear();
            return Ok(());
        }

        // Like the Linux IOMMU API, unmapping a part of a mapping is fine, and so is unmapping what
        // was never mapped.
        let affected = self.dma_bus.remove_range(address, size);
        if affected == 0 {
            debug!("dma_unmap of {address:#x}+{size:#x} did not hit any mapping");
        }

        Ok(())
    }

    fn reset(&mut self) -> Result<(), std::io::Error> {
        info!("Device reset requested");
        self.controller.reset();

        Ok(())
    }

    fn set_irqs(
        &mut self,
        index: u32,
        flags: u32,
        start: u32,
        count: u32,
        fds: Vec<File>,
    ) -> Result<(), std::io::Error> {
        debug!(
            "set IRQs: {index} flags: {flags:#x} start: {start:#x} count: {count:#x} #fds: {}",
            fds.len()
        );

        if index != VFIO_PCI_MSIX_IRQ_INDEX {
            // We report no other interrupts, but a client may still switch them off.
            debug!("ignoring IRQ configuration for index {index}");
            return Ok(());
        }

        if flags & VFIO_IRQ_SET_ACTION_TRIGGER == 0 {
            // Masking is up to the client: it only passes unmasked vectors to us.
            debug!("ignoring IRQ configuration without trigger action");
            return Ok(());
        }

        if flags & VFIO_IRQ_SET_DATA_NONE != 0 {
            if count == 0 {
                // Disable all vectors.
                self.controller
                    .connect_irq(Arc::new(DummyInterruptLine::default()));
            } else {
                debug!("ignoring request to trigger an interrupt");
            }
            return Ok(());
        }

        if flags & VFIO_IRQ_SET_DATA_EVENTFD == 0 {
            debug!("ignoring IRQ configuration with unsupported data type");
            return Ok(());
        }

        // The controller has a single interrupter, so there is only vector 0. The client sends
        // either a file descriptor per vector or none for all vectors to disable.
        if start != 0 || count == 0 {
            debug!("ignoring IRQ configuration for vectors from {start} on");
            return Ok(());
        }

        // The solution offered by clippy produces a type error.
        #[allow(clippy::option_if_let_else)]
        let irq: Arc<dyn InterruptLine> = match fds.into_iter().next() {
            Some(file) => Arc::new(InterruptEventFd {
                fd: Mutex::new(file),
            }),
            None => Arc::new(DummyInterruptLine::default()),
        };

        self.controller.connect_irq(irq);

        Ok(())
    }
}
