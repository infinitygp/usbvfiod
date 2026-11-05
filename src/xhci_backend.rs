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
        let result = self.fd.lock().unwrap().write(&1u64.to_le_bytes());
        if let Err(err) = result {
            warn!("failed to signal an interrupt: {err}");
        }
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
        // Reset while the guest memory is still mapped, so that the workers can finish their work.
        self.controller.reset();
        self.controller
            .connect_irq(Arc::new(DummyInterruptLine::default()));
        self.dma_bus.clear();
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

/// Split an access into pieces of 4, 2 or 1 byte, which is what the controller can serve.
///
/// The registers of the xHC are 32 bits wide, and 64-bit registers consist of two of them.
/// Splitting an 8-byte access is what the specification allows for a controller without 64-bit
/// addressing.
fn split_access(offset: u64, len: usize) -> impl Iterator<Item = (u64, usize, usize)> {
    let mut done = 0;
    std::iter::from_fn(move || {
        let remaining = len - done;
        let size = match remaining {
            0 => return None,
            1 => 1,
            2 | 3 => 2,
            _ => MAX_ACCESS_SIZE,
        };
        let chunk = (offset.wrapping_add(done as u64), done, size);
        done += size;
        Some(chunk)
    })
}

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

        for (chunk_offset, start, chunk_len) in split_access(offset, data.len()) {
            let chunk = &mut data[start..start + chunk_len];
            let Ok(size) = RequestSize::try_from(chunk_len) else {
                return;
            };
            let req = Request::new(chunk_offset, size);
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

        for (chunk_offset, start, chunk_len) in split_access(offset, data.len()) {
            let chunk = &data[start..start + chunk_len];
            let Ok(size) = RequestSize::try_from(chunk_len) else {
                return;
            };
            let req = Request::new(chunk_offset, size);
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
                // processes. A driver does not use it as DMA buffer as long as the RAM is shared.
                // The segment warns about the first access.
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

#[cfg(test)]
mod tests {
    use std::{ffi::CString, os::fd::FromRawFd};

    use vfio_bindings::bindings::vfio::{VFIO_IRQ_SET_DATA_NONE, VFIO_PCI_INTX_IRQ_INDEX};
    use vfio_user::{DmaMapFlags, DmaUnmapFlags};

    use crate::device::{
        bus::BusDevice,
        pci::constants::xhci::{capability, offset},
        xhci::real_device::{tests::testutils::MockRealDevice, CompleteRealDeviceImpl},
    };

    use super::*;

    type TestBackend = XhciBackend<CompleteRealDeviceImpl<MockRealDevice, (u8, u8)>>;

    fn backend() -> TestBackend {
        TestBackend::new(runtime::Handle::current()).unwrap()
    }

    fn memfd(size: u64, fill: impl Fn(u64) -> u8) -> File {
        // SAFETY: syscall with valid parameters.
        let fd = unsafe { libc::memfd_create(CString::new("unittest").unwrap().as_ptr(), 0) };
        assert!(fd >= 0);
        // SAFETY: fd is a valid file descriptor, because we created it above.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size).unwrap();
        let content: Vec<u8> = (0..size).map(fill).collect();
        std::os::unix::fs::FileExt::write_all_at(&file, &content, 0).unwrap();
        file
    }

    fn dma_read(backend: &TestBackend, address: u64) -> u64 {
        backend
            .dma_bus
            .read(Request::new(address, RequestSize::Size1))
    }

    fn read_region(backend: &mut TestBackend, region: u32, offset: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0; len];
        backend.region_read(region, offset, &mut data).unwrap();
        data
    }

    #[tokio::test]
    async fn dma_mapping_with_file_descriptor_is_accessible_at_its_address() {
        let mut backend = backend();
        let file = memfd(0x4000, |offset| (offset / 0x1000) as u8 + 1);

        // Map the second and third page of the file.
        backend
            .dma_map(
                DmaMapFlags::READ_WRITE,
                0x1000,
                0x8000_0000,
                0x2000,
                Some(file),
            )
            .unwrap();

        assert_eq!(dma_read(&backend, 0x8000_0000), 2);
        assert_eq!(dma_read(&backend, 0x8000_1fff), 3);
        assert_eq!(dma_read(&backend, 0x8000_2000), 0xff);
    }

    #[tokio::test]
    async fn dma_mapping_without_file_descriptor_is_tracked_and_fails_accesses_softly() {
        let mut backend = backend();

        backend
            .dma_map(DmaMapFlags::READ_WRITE, 0, 0xa0000, 0x10000, None)
            .unwrap();

        // Reads behave like an access that nobody answers, and writes do not panic.
        assert_eq!(dma_read(&backend, 0xa0000), 0xff);
        backend
            .dma_bus
            .write(Request::new(0xa0010, RequestSize::Size4), 0x1234);
        let mut data = [0u8; 8];
        backend.dma_bus.read_bulk(0xa0100, &mut data);
        assert_eq!(data, [0xff; 8]);

        // The range is taken: the client cannot map something else there.
        assert!(backend
            .dma_map(DmaMapFlags::READ_WRITE, 0, 0xa8000, 0x1000, None)
            .is_err());

        backend
            .dma_unmap(DmaUnmapFlags::empty(), 0xa0000, 0x10000)
            .unwrap();
        backend
            .dma_map(DmaMapFlags::READ_WRITE, 0, 0xa8000, 0x1000, None)
            .unwrap();
    }

    #[tokio::test]
    async fn dma_unmap_of_a_part_keeps_the_rest_of_the_mapping() {
        let mut backend = backend();
        backend
            .dma_map(
                DmaMapFlags::READ_WRITE,
                0,
                0x10000,
                0x4000,
                Some(memfd(0x4000, |offset| (offset / 0x1000) as u8 + 1)),
            )
            .unwrap();

        // The client removes the middle, like it does when it changes its memory map.
        backend
            .dma_unmap(DmaUnmapFlags::empty(), 0x11000, 0x2000)
            .unwrap();
        assert_eq!(dma_read(&backend, 0x10fff), 1);
        assert_eq!(dma_read(&backend, 0x11000), 0xff);
        assert_eq!(dma_read(&backend, 0x12fff), 0xff);
        assert_eq!(dma_read(&backend, 0x13000), 4);

        // The hole can be filled again.
        backend
            .dma_map(
                DmaMapFlags::READ_WRITE,
                0,
                0x11000,
                0x2000,
                Some(memfd(0x2000, |_| 0x77)),
            )
            .unwrap();
        assert_eq!(dma_read(&backend, 0x11000), 0x77);

        // An unmap that spans several mappings and goes beyond them.
        backend
            .dma_unmap(DmaUnmapFlags::empty(), 0, 0x100000)
            .unwrap();
        assert_eq!(dma_read(&backend, 0x10000), 0xff);
        assert_eq!(dma_read(&backend, 0x13000), 0xff);
    }

    #[tokio::test]
    async fn dma_unmap_all_forgets_every_mapping() {
        let mut backend = backend();
        for address in [0x1000, 0x5000] {
            backend
                .dma_map(
                    DmaMapFlags::READ_WRITE,
                    0,
                    address,
                    0x1000,
                    Some(memfd(0x1000, |_| 1)),
                )
                .unwrap();
        }

        backend.dma_unmap(DmaUnmapFlags::UNMAP_ALL, 0, 0).unwrap();

        assert_eq!(dma_read(&backend, 0x1000), 0xff);
        assert_eq!(dma_read(&backend, 0x5000), 0xff);
        backend
            .dma_unmap(DmaUnmapFlags::empty(), 0x1000, 0x1000)
            .unwrap();
    }

    #[tokio::test]
    async fn split_access_covers_every_size_with_valid_pieces() {
        let pieces: Vec<_> = split_access(0x10, 7).collect();
        assert_eq!(pieces, [(0x10, 0, 4), (0x14, 4, 2), (0x16, 6, 1)]);
        assert_eq!(split_access(0, 8).count(), 2);
        assert_eq!(split_access(0, 0).count(), 0);
    }

    #[tokio::test]
    async fn config_space_identifies_an_xhci_controller() {
        let mut backend = backend();

        let ids = read_region(&mut backend, VFIO_PCI_CONFIG_REGION_INDEX, 0, 4);
        assert_eq!(ids, [0x36, 0x1b, 0x0d, 0x00]);
        let class = read_region(&mut backend, VFIO_PCI_CONFIG_REGION_INDEX, 9, 3);
        assert_eq!(class, [0x30, 0x03, 0x0c]);
    }

    #[tokio::test]
    async fn accesses_beyond_a_region_or_to_unknown_regions_do_not_panic() {
        let mut backend = backend();

        assert_eq!(
            read_region(&mut backend, VFIO_PCI_CONFIG_REGION_INDEX, 254, 4),
            [0xff; 4]
        );
        assert_eq!(read_region(&mut backend, 6, 0, 4), [0xff; 4]);
        assert_eq!(read_region(&mut backend, 42, 0, 4), [0xff; 4]);
        assert_eq!(read_region(&mut backend, 0, u64::MAX - 1, 4), [0xff; 4]);
        assert_eq!(read_region(&mut backend, 0, 0, 0), Vec::<u8>::new());

        backend
            .region_write(VFIO_PCI_CONFIG_REGION_INDEX, 255, &[1, 2, 3, 4])
            .unwrap();
        backend.region_write(42, 0, &[1, 2, 3, 4]).unwrap();
        backend.region_write(0, 1 << 40, &[1, 2, 3, 4]).unwrap();
        backend.region_write(0, 0, &[1, 2, 3]).unwrap();
        backend.region_write(0, 0x3ffc, &[0; 4]).unwrap();
    }

    #[tokio::test]
    async fn the_pending_bit_array_of_msix_reads_as_zero() {
        let mut backend = backend();

        assert_eq!(
            read_region(&mut backend, VFIO_PCI_BAR3_REGION_INDEX, 0x1000, 8),
            [0; 8]
        );
    }

    #[tokio::test]
    async fn eight_byte_accesses_to_the_mmio_region_reach_both_halves_of_a_register() {
        let mut backend = backend();

        // CAPLENGTH/HCIVERSION and HCSPARAMS1 in one access.
        let data = read_region(
            &mut backend,
            VFIO_PCI_BAR0_REGION_INDEX,
            offset::CAPLENGTH,
            8,
        );
        let caplength_hciversion = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let hcsparams1 = u32::from_le_bytes(data[4..8].try_into().unwrap());
        assert_eq!(caplength_hciversion & 0xff, 0x40);
        assert_eq!(
            u64::from(caplength_hciversion >> 16),
            capability::HCIVERSION
        );
        assert_eq!(u64::from(hcsparams1), capability::HCSPARAMS1);

        // A 64-bit register, written as a whole.
        backend
            .region_write(
                VFIO_PCI_BAR0_REGION_INDEX,
                offset::DCBAAP,
                &0x1234_5000u64.to_le_bytes(),
            )
            .unwrap();
        let data = read_region(&mut backend, VFIO_PCI_BAR0_REGION_INDEX, offset::DCBAAP, 8);
        assert_eq!(u64::from_le_bytes(data.try_into().unwrap()), 0x1234_5000);
    }

    #[tokio::test]
    async fn registers_that_do_not_exist_read_as_zero_and_ignore_writes() {
        let mut backend = backend();

        assert_eq!(
            read_region(&mut backend, VFIO_PCI_BAR0_REGION_INDEX, 0x1000, 4),
            [0; 4]
        );
        backend
            .region_write(VFIO_PCI_BAR0_REGION_INDEX, 0x1000, &[1, 2, 3, 4])
            .unwrap();
        // The upper half of an address register is not backed either.
        backend
            .region_write(VFIO_PCI_BAR0_REGION_INDEX, offset::CRCR_HI, &[1, 0, 0, 0])
            .unwrap();
        // DNCTL only has defined values.
        backend
            .region_write(VFIO_PCI_BAR0_REGION_INDEX, offset::DNCTL, &[0xff; 4])
            .unwrap();
    }

    #[tokio::test]
    async fn interrupt_requests_of_the_client_never_panic() {
        let mut backend = backend();

        // Switch off everything, like a client does when it resets the device.
        for index in 0..VFIO_PCI_NUM_IRQS {
            backend
                .set_irqs(
                    index,
                    VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
                    0,
                    0,
                    vec![],
                )
                .unwrap();
        }
        // More vectors than we have, and a vector that does not exist.
        backend
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                4,
                vec![memfd(8, |_| 0), memfd(8, |_| 0)],
            )
            .unwrap();
        backend
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
                7,
                1,
                vec![memfd(8, |_| 0)],
            )
            .unwrap();
        // Interrupt types we do not offer, masking and triggering by the client.
        backend
            .set_irqs(
                VFIO_PCI_INTX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                1,
                vec![],
            )
            .unwrap();
        backend
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_NONE,
                0,
                1,
                vec![],
            )
            .unwrap();
        backend
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                1,
                vec![],
            )
            .unwrap();
    }

    // The reset waits for the workers of the controller, so they need threads of their own.
    #[tokio::test(flavor = "multi_thread")]
    async fn device_reset_restores_the_initial_state() {
        let mut backend = backend();
        backend
            .dma_map(
                DmaMapFlags::READ_WRITE,
                0,
                0x1000,
                0x1000,
                Some(memfd(0x1000, |_| 9)),
            )
            .unwrap();

        // The client programs the first BAR and the driver the DCBAAP.
        backend
            .region_write(
                VFIO_PCI_CONFIG_REGION_INDEX,
                0x10,
                &0xfe00_0000u32.to_le_bytes(),
            )
            .unwrap();
        backend
            .region_write(
                VFIO_PCI_BAR0_REGION_INDEX,
                offset::DCBAAP,
                &0x1000u32.to_le_bytes(),
            )
            .unwrap();
        assert_eq!(
            read_region(&mut backend, VFIO_PCI_CONFIG_REGION_INDEX, 0x10, 4),
            0xfe00_0000u32.to_le_bytes()
        );

        backend.reset().unwrap();

        assert_eq!(
            read_region(&mut backend, VFIO_PCI_CONFIG_REGION_INDEX, 0x10, 4),
            [0; 4]
        );
        assert_eq!(
            read_region(&mut backend, VFIO_PCI_BAR0_REGION_INDEX, offset::DCBAAP, 4),
            [0; 4]
        );
        // The memory of the client is not forgotten by a reset of the device.
        assert_eq!(dma_read(&backend, 0x1000), 9);

        // Losing the connection is different.
        backend.disconnect();
        assert_eq!(dma_read(&backend, 0x1000), 0xff);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_on_the_socket_maps_memory_resets_and_unmaps() {
        use std::os::fd::AsRawFd;

        let socket_path =
            std::env::temp_dir().join(format!("usbvfiod-test-{}", std::process::id()));
        let mut backend = backend();
        let server =
            vfio_user::Server::new(&socket_path, true, backend.irqs(), backend.regions()).unwrap();
        let server_thread = std::thread::spawn(move || {
            let result = server.run(&mut backend);
            (result, backend)
        });

        let mut client = vfio_user::Client::new(&socket_path).unwrap();
        assert_eq!(
            client.region(VFIO_PCI_BAR0_REGION_INDEX).unwrap().size,
            0x4000
        );

        // The memory of the client arrives as file descriptor.
        let ram = memfd(0x4000, |offset| (offset / 0x1000) as u8 + 1);
        client.dma_map(0, 0x10000, 0x4000, ram.as_raw_fd()).unwrap();

        let mut vendor_and_device = [0u8; 4];
        client
            .region_read(VFIO_PCI_CONFIG_REGION_INDEX, 0, &mut vendor_and_device)
            .unwrap();
        assert_eq!(vendor_and_device, [0x36, 0x1b, 0x0d, 0x00]);

        client
            .region_write(
                VFIO_PCI_BAR0_REGION_INDEX,
                offset::DCBAAP,
                &0x2000u32.to_le_bytes(),
            )
            .unwrap();
        let mut dcbaap = [0u8; 8];
        client
            .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset::DCBAAP, &mut dcbaap)
            .unwrap();
        assert_eq!(u64::from_le_bytes(dcbaap), 0x2000);

        // The client enables the interrupt with an event file descriptor and disables it again.
        client
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                1,
                &[ram.as_raw_fd()],
            )
            .unwrap();
        client
            .set_irqs(
                VFIO_PCI_MSIX_IRQ_INDEX,
                VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                0,
                &[],
            )
            .unwrap();

        client.reset().unwrap();
        client
            .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset::DCBAAP, &mut dcbaap)
            .unwrap();
        assert_eq!(u64::from_le_bytes(dcbaap), 0);

        // Unmap a page in the middle of the mapping, like QEMU does when it changes its memory map.
        client.dma_unmap(0x11000, 0x1000).unwrap();

        // Closing the connection ends the server and gives the backend back.
        drop(client);
        let (result, backend) = server_thread.join().unwrap();
        result.unwrap();
        assert_eq!(dma_read(&backend, 0x10000), 1);
        assert_eq!(dma_read(&backend, 0x11000), 0xff);
        assert_eq!(dma_read(&backend, 0x12000), 3);
        assert!(!socket_path.exists(), "the server removes its socket");
    }

    /// Serves a virtual HID device to a vfio-user client until the process is killed.
    ///
    /// This is for checks with a real guest when no hardware is at hand:
    ///
    /// ```console
    /// $ USBVFIOD_TEST_SOCKET=$PWD/S cargo test serve_a_virtual_hid_to_a_guest -- --ignored --nocapture
    /// ```
    ///
    /// With `USBVFIOD_TEST_FLAP_SECS=N` the device is detached and attached again every N seconds.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "runs until it is killed"]
    async fn serve_a_virtual_hid_to_a_guest() {
        use crate::device::xhci::virtual_hid::VirtualHid;

        let socket_path = std::env::var_os("USBVFIOD_TEST_SOCKET")
            .expect("USBVFIOD_TEST_SOCKET names the socket to listen on");
        let flap_interval = std::env::var("USBVFIOD_TEST_FLAP_SECS")
            .ok()
            .map(|secs| std::time::Duration::from_secs(secs.parse().unwrap()));

        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let mut backend = XhciBackend::<CompleteRealDeviceImpl<VirtualHid, (u8, u8)>>::new(
            runtime::Handle::current(),
        )
        .unwrap();
        let hotplug_control = backend.hotplug_control();
        async fn attach(control: &HotplugControl<CompleteRealDeviceImpl<VirtualHid, (u8, u8)>>) {
            let device = CompleteRealDeviceImpl::new((9, 9), VirtualHid);
            info!("attach: {:?}", control.attach(device).await);
        }
        attach(&hotplug_control).await;

        if let Some(interval) = flap_interval {
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    info!("detach: {:?}", hotplug_control.detach((9, 9)).await);
                    tokio::time::sleep(interval).await;
                    attach(&hotplug_control).await;
                }
            });
        }

        let server = vfio_user::Server::new(
            std::path::Path::new(&socket_path),
            true,
            backend.irqs(),
            backend.regions(),
        )
        .unwrap();
        loop {
            info!(
                "serve: {:?}",
                server.run(&mut backend).map_err(|e| e.to_string())
            );
            backend.disconnect();
        }
    }
}
