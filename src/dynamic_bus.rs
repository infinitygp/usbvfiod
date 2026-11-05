use std::sync::{Arc, Mutex};

use crate::device::bus::{AddBusDeviceError, Bus, BusDevice, BusDeviceRef, Request};
use arc_swap::ArcSwap;

#[derive(Debug, Clone)]
struct DeviceEntry {
    start_addr: u64,
    device: BusDeviceRef,
}

impl DeviceEntry {
    fn end_addr(&self) -> u64 {
        self.start_addr.saturating_add(self.device.size())
    }
}

/// A window into another bus device.
///
/// This is what remains of a mapping after a part of it was removed.
#[derive(Debug)]
struct DeviceSlice {
    device: BusDeviceRef,
    offset: u64,
    size: u64,
}

impl BusDevice for DeviceSlice {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, req: Request) -> u64 {
        self.device
            .read(Request::new(self.offset + req.addr, req.size))
    }

    fn write(&self, req: Request, value: u64) {
        self.device
            .write(Request::new(self.offset + req.addr, req.size), value);
    }

    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        self.device.read_bulk(self.offset + offset, data);
    }

    fn write_bulk(&self, offset: u64, data: &[u8]) {
        self.device.write_bulk(self.offset + offset, data);
    }

    fn compare_exchange_request(&self, req: Request, current: u64, new: u64) -> Result<u64, u64> {
        self.device.compare_exchange_request(
            Request::new(self.offset + req.addr, req.size),
            current,
            new,
        )
    }
}

/// A bus whose devices can be added and removed while it is in use.
///
/// Accesses never wait for an update. They see either the bus before or the
/// bus after it.
#[derive(Default, Debug)]
pub struct DynamicBus {
    segments: Mutex<Vec<DeviceEntry>>,
    bus: Arc<ArcSwap<Bus>>,
}

impl DynamicBus {
    pub fn new() -> Self {
        Default::default()
    }

    fn build_bus(segments: &[DeviceEntry]) -> Result<Bus, AddBusDeviceError> {
        let mut new_bus = Bus::new("DMA bus", u64::MAX);
        for segment in segments {
            new_bus.add(segment.start_addr, segment.device.clone())?;
        }
        Ok(new_bus)
    }

    /// Add a device that claims `device.size()` bytes starting at `start_addr`.
    ///
    /// The bus is unchanged if the range overlaps an existing device.
    pub fn add(&self, start_addr: u64, device: BusDeviceRef) -> Result<(), AddBusDeviceError> {
        let mut segments = self.segments.lock().unwrap();

        let mut new_segments = segments.clone();
        new_segments.push(DeviceEntry { start_addr, device });
        let new_bus = Self::build_bus(&new_segments)?;

        *segments = new_segments;

        // It's okay to use store here, because we only have a single
        // writer (serialized by the mutex).
        self.bus.store(Arc::new(new_bus));

        // Silence clippy: we want updates to `self.bus` also to be synchronized
        // by the Mutex.
        drop(segments);

        Ok(())
    }

    /// Remove everything that is claimed in `start_addr..start_addr + size`.
    ///
    /// A device that is only partly inside the range stays on the bus with
    /// the part that is outside. Returns the number of devices that were
    /// affected.
    pub fn remove_range(&self, start_addr: u64, size: u64) -> usize {
        if size == 0 {
            return 0;
        }

        let end_addr = start_addr.saturating_add(size);
        let mut segments = self.segments.lock().unwrap();

        let mut affected = 0;
        let mut new_segments = Vec::with_capacity(segments.len());
        for entry in segments.iter() {
            if entry.start_addr >= end_addr || entry.end_addr() <= start_addr {
                new_segments.push(entry.clone());
                continue;
            }

            affected += 1;
            if entry.start_addr < start_addr {
                new_segments.push(DeviceEntry {
                    start_addr: entry.start_addr,
                    device: Arc::new(DeviceSlice {
                        device: entry.device.clone(),
                        offset: 0,
                        size: start_addr - entry.start_addr,
                    }),
                });
            }
            if entry.end_addr() > end_addr {
                new_segments.push(DeviceEntry {
                    start_addr: end_addr,
                    device: Arc::new(DeviceSlice {
                        device: entry.device.clone(),
                        offset: end_addr - entry.start_addr,
                        size: entry.end_addr() - end_addr,
                    }),
                });
            }
        }

        if affected > 0 {
            // Removing and shrinking cannot create an overlap or an out of range device.
            let new_bus = Self::build_bus(&new_segments).expect("a shrunk bus is always valid");
            *segments = new_segments;
            self.bus.store(Arc::new(new_bus));
        }
        drop(segments);

        affected
    }

    /// Remove all devices.
    pub fn clear(&self) {
        let mut segments = self.segments.lock().unwrap();
        segments.clear();
        self.bus.store(Arc::new(Bus::new("DMA bus", u64::MAX)));
        drop(segments);
    }
}

impl BusDevice for DynamicBus {
    fn size(&self) -> u64 {
        self.bus.load().size()
    }

    fn read(&self, req: Request) -> u64 {
        self.bus.load().read(req)
    }

    fn write(&self, req: Request, value: u64) {
        self.bus.load().write(req, value);
    }

    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        self.bus.load().read_bulk(offset, data);
    }

    fn write_bulk(&self, offset: u64, data: &[u8]) {
        self.bus.load().write_bulk(offset, data);
    }

    fn compare_exchange_request(&self, req: Request, current: u64, new: u64) -> Result<u64, u64> {
        self.bus.load().compare_exchange_request(req, current, new)
    }
}

#[cfg(test)]
mod tests {
    use crate::device::bus::testutils::TestBusDevice;
    use crate::device::bus::RequestSize;

    use super::*;

    #[test]
    fn can_add_devices() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestBusDevice::new(&[42u8; 0x1000]));

        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 0xFF);

        bus.add(0x1000, device1).unwrap();
        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 42);
    }

    #[test]
    fn overlapping_add_leaves_bus_unchanged() {
        let bus = DynamicBus::default();
        bus.add(0x1000, Arc::new(TestBusDevice::new(&[1u8; 0x1000])))
            .unwrap();

        assert!(bus
            .add(0x1800, Arc::new(TestBusDevice::new(&[2u8; 0x1000])))
            .is_err());

        // A failed add must not poison later updates.
        bus.add(0x3000, Arc::new(TestBusDevice::new(&[3u8; 0x1000])))
            .unwrap();
        assert_eq!(bus.read(Request::new(0x1800, RequestSize::Size1)), 1);
        assert_eq!(bus.read(Request::new(0x3800, RequestSize::Size1)), 3);
    }

    fn bus_with_pattern() -> DynamicBus {
        // Each page of the device holds its page number + 1, so reads show where they were routed.
        let data: Vec<u8> = (0..0x4000u32).map(|i| (i / 0x1000) as u8 + 1).collect();
        let bus = DynamicBus::default();
        bus.add(0x10000, Arc::new(TestBusDevice::new(&data)))
            .unwrap();
        bus
    }

    fn byte_at(bus: &DynamicBus, addr: u64) -> u64 {
        bus.read(Request::new(addr, RequestSize::Size1))
    }

    #[test]
    fn remove_whole_device() {
        let bus = bus_with_pattern();

        assert_eq!(bus.remove_range(0x10000, 0x4000), 1);
        assert_eq!(byte_at(&bus, 0x10000), 0xFF);
        assert_eq!(byte_at(&bus, 0x13fff), 0xFF);
    }

    #[test]
    fn remove_range_in_the_middle_keeps_both_sides() {
        let bus = bus_with_pattern();

        assert_eq!(bus.remove_range(0x11000, 0x2000), 1);
        assert_eq!(byte_at(&bus, 0x10fff), 1);
        assert_eq!(byte_at(&bus, 0x11000), 0xFF);
        assert_eq!(byte_at(&bus, 0x12fff), 0xFF);
        assert_eq!(byte_at(&bus, 0x13000), 4);
        assert_eq!(byte_at(&bus, 0x13fff), 4);
    }

    #[test]
    fn remove_range_at_the_edges_and_beyond() {
        let bus = bus_with_pattern();

        // Starts before the device and ends inside.
        assert_eq!(bus.remove_range(0xf000, 0x3000), 1);
        assert_eq!(byte_at(&bus, 0x11fff), 0xFF);
        assert_eq!(byte_at(&bus, 0x12000), 3);

        // Starts inside and ends after the device.
        assert_eq!(bus.remove_range(0x13000, 0x10000), 1);
        assert_eq!(byte_at(&bus, 0x12fff), 3);
        assert_eq!(byte_at(&bus, 0x13000), 0xFF);
    }

    #[test]
    fn remove_range_without_device_changes_nothing() {
        let bus = bus_with_pattern();

        assert_eq!(bus.remove_range(0x20000, 0x1000), 0);
        assert_eq!(bus.remove_range(0x0, 0x10000), 0);
        assert_eq!(byte_at(&bus, 0x10000), 1);
    }

    #[test]
    fn remove_empty_range_changes_nothing() {
        let bus = bus_with_pattern();

        assert_eq!(bus.remove_range(0x11000, 0), 0);
        assert_eq!(byte_at(&bus, 0x10fff), 1);
        assert_eq!(byte_at(&bus, 0x11000), 2);
    }

    #[test]
    fn remove_range_spanning_several_devices() {
        let bus = DynamicBus::default();
        bus.add(0x0, Arc::new(TestBusDevice::new(&[1u8; 0x1000])))
            .unwrap();
        bus.add(0x1000, Arc::new(TestBusDevice::new(&[2u8; 0x1000])))
            .unwrap();
        bus.add(0x2000, Arc::new(TestBusDevice::new(&[3u8; 0x1000])))
            .unwrap();

        assert_eq!(bus.remove_range(0x800, 0x1800), 2);
        assert_eq!(byte_at(&bus, 0x7ff), 1);
        assert_eq!(byte_at(&bus, 0x800), 0xFF);
        assert_eq!(byte_at(&bus, 0x1fff), 0xFF);
        assert_eq!(byte_at(&bus, 0x2000), 3);
        assert_eq!(byte_at(&bus, 0x2800), 3);
    }

    #[test]
    fn slices_forward_bulk_accesses_with_the_right_offset() {
        let data: Vec<u8> = (0..0x2000usize).map(|i| (i % 251) as u8).collect();
        let bus = DynamicBus::default();
        bus.add(0x4000, Arc::new(TestBusDevice::new(&data)))
            .unwrap();
        bus.remove_range(0x4000, 0x1000);

        let mut read = [0u8; 16];
        bus.read_bulk(0x5100, &mut read);
        assert_eq!(&read[..], &data[0x1100..0x1110]);

        bus.write_bulk(0x5200, &[0xAA; 4]);
        assert_eq!(
            bus.read(Request::new(0x5200, RequestSize::Size4)),
            0xAAAAAAAA
        );
    }

    #[test]
    fn clear_removes_everything() {
        let bus = bus_with_pattern();

        bus.clear();
        assert_eq!(byte_at(&bus, 0x10000), 0xFF);

        // The bus is still usable.
        bus.add(0x10000, Arc::new(TestBusDevice::new(&[7u8; 0x1000])))
            .unwrap();
        assert_eq!(byte_at(&bus, 0x10000), 7);
    }
}
