//! A minimal virtual HID device for checking the controller with a real guest.
//!
//! It enumerates like a USB input device with a vendor defined report, and sends a report every
//! few hundred milliseconds so that the guest sees transfer events all the time. The hotplug
//! command `--virtual-hid` attaches it at a port path, at full speed or at high speed, and the
//! ignored test `serve_a_virtual_hid_to_a_guest` in `xhci_backend` serves it to a guest without any
//! hardware or hotplug socket.

use std::{future::Future, pin::Pin, time::Duration};

use tokio::time::sleep;

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
        RealControlEndpointHandle, RealInEndpointHandle,
    },
    stalling_endpoint::{StallingInEndpoint, StallingOutEndpoint},
    usbrequest::UsbRequest,
};

/// How long the device waits between two reports.
const REPORT_INTERVAL: Duration = Duration::from_millis(250);

const REPORT_DESCRIPTOR: [u8; 21] = [
    0x06, 0x00, 0xff, // Usage Page (vendor defined)
    0x09, 0x01, // Usage (1)
    0xa1, 0x01, // Collection (Application)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xff, 0x00, //   Logical Maximum (255)
    0x75, 0x08, //   Report Size (8)
    0x95, 0x02, //   Report Count (2)
    0x09, 0x02, //   Usage (2)
    0x81, 0x02, //   Input (Data, Variable, Absolute)
    0xc0, // End Collection
];

/// The descriptors are those of a full speed only device. At high speed the device is a USB 2.0
/// device with the same descriptors, and a shorter interval of the interrupt endpoint.
const DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, // bLength, bDescriptorType
    0x10, 0x01, // bcdUSB 1.10
    0, 0, 0,  // class, subclass, protocol: defined by the interface
    64, // bMaxPacketSize0
    0x09, 0x12, // idVendor 0x1209
    0x00, 0xde, // idProduct 0xde00
    0x00, 0x01, // bcdDevice 1.00
    1, 2, 3, // manufacturer, product, serial number
    1, // bNumConfigurations
];

const CONFIGURATION_DESCRIPTOR: [u8; 34] = [
    9,
    2,
    34,
    0,
    1,
    1,
    0,
    0x80,
    50, // configuration with one interface
    9,
    4,
    0,
    0,
    1,
    3,
    0,
    0,
    0, // interface 0: HID with one endpoint
    9,
    0x21,
    0x11,
    0x01,
    0,
    1,
    0x22,
    REPORT_DESCRIPTOR.len() as u8,
    0, // HID descriptor
    7,
    5,
    0x81,
    3,
    2,
    0,
    10, // endpoint 1 IN, interrupt, 2 bytes, every 10 ms
];

fn string_descriptor(index: u8) -> Option<Vec<u8>> {
    let text = match index {
        0 => return Some(vec![4, 3, 0x09, 0x04]),
        1 => "usbvfiod",
        2 => "usbvfiod test HID",
        3 => "0001",
        _ => return None,
    };
    let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut descriptor = vec![units.len() as u8 + 2, 3];
    descriptor.extend(units);
    Some(descriptor)
}

/// A HID device with a vendor defined input report, at full speed or at high speed.
#[derive(Debug)]
pub struct VirtualHid {
    speed: Speed,
}

impl VirtualHid {
    pub const fn new(speed: Speed) -> Self {
        Self { speed }
    }
}

impl Default for VirtualHid {
    fn default() -> Self {
        Self::new(Speed::Full)
    }
}

impl RealDevice for VirtualHid {
    type RCEH = VirtualHidControlEndpoint;
    type RBIEH = StallingInEndpoint;
    type RBOEH = StallingOutEndpoint;
    type RIIEH = VirtualHidInterruptEndpoint;
    type RIOEH = StallingOutEndpoint;

    fn speed(&self) -> Option<Speed> {
        Some(self.speed)
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        VirtualHidControlEndpoint {
            speed: self.speed,
            response: None,
        }
    }

    fn bulk_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBIEH {
        StallingInEndpoint
    }

    fn bulk_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBOEH {
        StallingOutEndpoint
    }

    fn interrupt_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIIEH {
        VirtualHidInterruptEndpoint {
            requested: 0,
            counter: 0,
        }
    }

    fn interrupt_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIOEH {
        StallingOutEndpoint
    }
}

#[derive(Debug)]
pub struct VirtualHidControlEndpoint {
    speed: Speed,
    response: Option<ControlRequestProcessingResult>,
}

impl VirtualHidControlEndpoint {
    /// The device descriptor, which tells a high speed device from a full speed one by its USB
    /// version.
    fn device_descriptor(speed: Speed) -> Vec<u8> {
        let mut descriptor = DEVICE_DESCRIPTOR.to_vec();
        if speed == Speed::High {
            descriptor[2..4].copy_from_slice(&[0x00, 0x02]);
        }
        descriptor
    }

    /// The configuration descriptor, with the polling interval of the endpoint for the speed:
    /// 10 frames of 1 ms at full speed, and 8 ms (an exponent of 7) at high speed.
    fn configuration_descriptor(speed: Speed) -> Vec<u8> {
        let mut descriptor = CONFIGURATION_DESCRIPTOR.to_vec();
        if speed == Speed::High {
            *descriptor.last_mut().expect("the descriptor is not empty") = 7;
        }
        descriptor
    }

    /// The device qualifier and the configuration for the other speed, which a high speed device
    /// has and a full speed one does not.
    fn other_speed_descriptor(speed: Speed, descriptor_type: u8) -> Option<Vec<u8>> {
        if speed != Speed::High {
            return None;
        }
        match descriptor_type {
            6 => Some(vec![10, 6, 0x00, 0x02, 0, 0, 0, 64, 1, 0]),
            7 => {
                let mut descriptor = CONFIGURATION_DESCRIPTOR.to_vec();
                descriptor[1] = 7;
                Some(descriptor)
            }
            _ => None,
        }
    }

    fn answer(speed: Speed, request: &UsbRequest) -> ControlRequestProcessingResult {
        const GET_STATUS: u8 = 0;
        const GET_DESCRIPTOR: u8 = 6;
        const SET_CONFIGURATION: u8 = 9;
        const SET_INTERFACE: u8 = 11;
        const SET_IDLE: u8 = 0x0a;
        const SET_PROTOCOL: u8 = 0x0b;

        let descriptor_type = (request.value >> 8) as u8;
        let descriptor_index = (request.value & 0xff) as u8;

        let data = match (request.request_type, request.request) {
            (0x80, GET_DESCRIPTOR) => match descriptor_type {
                1 => Some(Self::device_descriptor(speed)),
                2 => Some(Self::configuration_descriptor(speed)),
                3 => string_descriptor(descriptor_index),
                other => Self::other_speed_descriptor(speed, other),
            },
            (0x81, GET_DESCRIPTOR) if descriptor_type == 0x22 => Some(REPORT_DESCRIPTOR.to_vec()),
            (0x80 | 0x81, GET_STATUS) => Some(vec![0, 0]),
            (0x00 | 0x01, SET_CONFIGURATION | SET_INTERFACE) | (0x21, SET_IDLE | SET_PROTOCOL) => {
                return ControlRequestProcessingResult::SuccessfulControlOut;
            }
            _ => None,
        };

        data.map_or(ControlRequestProcessingResult::Stall, |mut data| {
            data.truncate(request.length as usize);
            ControlRequestProcessingResult::SuccessfulControlIn(data)
        })
    }
}

impl RealControlEndpointHandle for VirtualHidControlEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.response = Some(Self::answer(self.speed, &request));
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            self.response
                .take()
                .ok_or_else(|| anyhow::anyhow!("no control request was submitted"))
        })
    }
}

impl BaseEndpointHandle for VirtualHidControlEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
pub struct VirtualHidInterruptEndpoint {
    requested: usize,
    counter: u8,
}

impl RealInEndpointHandle for VirtualHidInterruptEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, data: usize) -> anyhow::Result<()> {
        self.requested = data;
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            sleep(REPORT_INTERVAL).await;
            self.counter = self.counter.wrapping_add(1);
            let mut data = vec![self.counter, !self.counter];
            data.truncate(self.requested);
            Ok(InTrbProcessingResult {
                status: InTrbProcessingStatus::Success,
                data,
            })
        })
    }
}

impl BaseEndpointHandle for VirtualHidInterruptEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_at(
        speed: Speed,
        request_type: u8,
        request: u8,
        value: u16,
        length: u32,
    ) -> ControlRequestProcessingResult {
        VirtualHidControlEndpoint::answer(
            speed,
            &UsbRequest {
                request_type,
                request,
                value,
                length,
                ..Default::default()
            },
        )
    }

    fn control(
        request_type: u8,
        request: u8,
        value: u16,
        length: u32,
    ) -> ControlRequestProcessingResult {
        control_at(Speed::Full, request_type, request, value, length)
    }

    fn data_of(result: ControlRequestProcessingResult) -> Vec<u8> {
        match result {
            ControlRequestProcessingResult::SuccessfulControlIn(data) => data,
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    fn descriptors_are_consistent() {
        assert_eq!(data_of(control(0x80, 6, 0x0100, 64)), DEVICE_DESCRIPTOR);
        assert_eq!(
            data_of(control(0x80, 6, 0x0100, 8)),
            DEVICE_DESCRIPTOR[..8],
            "the answer is cut to wLength"
        );

        let configuration = data_of(control(0x80, 6, 0x0200, 255));
        assert_eq!(
            usize::from(u16::from_le_bytes([configuration[2], configuration[3]])),
            configuration.len()
        );
        assert_eq!(data_of(control(0x81, 6, 0x2200, 255)), REPORT_DESCRIPTOR);

        let product = data_of(control(0x80, 6, 0x0302, 255));
        assert_eq!(usize::from(product[0]), product.len());
        assert_eq!(data_of(control(0x80, 6, 0x0300, 255)), [4, 3, 9, 4]);
    }

    #[test]
    fn a_high_speed_device_is_a_usb_2_device_with_other_speed_descriptors() {
        let device = data_of(control_at(Speed::High, 0x80, 6, 0x0100, 64));
        assert_eq!(device[2..4], [0x00, 0x02], "bcdUSB 2.00");
        assert_eq!(device[4..], DEVICE_DESCRIPTOR[4..]);

        let configuration = data_of(control_at(Speed::High, 0x80, 6, 0x0200, 255));
        assert_eq!(configuration[..33], CONFIGURATION_DESCRIPTOR[..33]);
        assert_eq!(configuration[33], 7, "the interval is an exponent");

        assert_eq!(
            data_of(control_at(Speed::High, 0x80, 6, 0x0600, 255)),
            [10, 6, 0x00, 0x02, 0, 0, 0, 64, 1, 0]
        );
        let other_speed = data_of(control_at(Speed::High, 0x80, 6, 0x0700, 255));
        assert_eq!(other_speed[1], 7);
        assert_eq!(other_speed[2..], CONFIGURATION_DESCRIPTOR[2..]);
    }

    #[test]
    fn a_full_speed_device_does_not_know_about_other_speeds() {
        assert!(matches!(
            control(0x80, 6, 0x0600, 255),
            ControlRequestProcessingResult::Stall
        ));
        assert!(matches!(
            control(0x80, 6, 0x0700, 255),
            ControlRequestProcessingResult::Stall
        ));
        assert_eq!(data_of(control(0x80, 6, 0x0100, 64))[2..4], [0x10, 0x01]);
    }

    #[test]
    fn unknown_requests_stall() {
        assert!(matches!(
            control(0x80, 6, 0x0900, 255),
            ControlRequestProcessingResult::Stall
        ));
        assert!(matches!(
            control(0x40, 0x42, 0, 0),
            ControlRequestProcessingResult::Stall
        ));
        assert!(matches!(
            control(0x00, 9, 1, 0),
            ControlRequestProcessingResult::SuccessfulControlOut
        ));
    }
}
