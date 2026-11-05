//! A minimal virtual HID device for checking the controller with a real guest.
//!
//! The device is only compiled for tests. The ignored test `serve_a_virtual_hid_to_a_guest` in
//! `xhci_backend` serves it to a guest without any hardware: it enumerates like a USB input
//! device with a vendor defined report, and sends a report every few hundred milliseconds so that
//! the guest sees transfer events all the time.

use std::{future::Future, pin::Pin, time::Duration};

use tokio::time::sleep;

use crate::device::xhci::{
    endpoint_handle::tests::testutils::{MockRealInEndpoint, MockRealOutEndpoint},
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
        RealControlEndpointHandle, RealInEndpointHandle,
    },
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

const DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, // bLength, bDescriptorType
    0x10, 0x01, // bcdUSB 1.10, a full speed only device
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

/// A full speed HID device with a vendor defined input report.
#[derive(Debug, Default)]
pub struct VirtualHid;

impl RealDevice for VirtualHid {
    type RCEH = VirtualHidControlEndpoint;
    type RBIEH = MockRealInEndpoint;
    type RBOEH = MockRealOutEndpoint;
    type RIIEH = VirtualHidInterruptEndpoint;
    type RIOEH = MockRealOutEndpoint;

    fn speed(&self) -> Option<Speed> {
        Some(Speed::Full)
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        VirtualHidControlEndpoint { response: None }
    }

    fn bulk_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBIEH {
        MockRealInEndpoint::new(None)
    }

    fn bulk_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBOEH {
        MockRealOutEndpoint::new()
    }

    fn interrupt_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIIEH {
        VirtualHidInterruptEndpoint {
            requested: 0,
            counter: 0,
        }
    }

    fn interrupt_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIOEH {
        MockRealOutEndpoint::new()
    }
}

#[derive(Debug)]
pub struct VirtualHidControlEndpoint {
    response: Option<ControlRequestProcessingResult>,
}

impl VirtualHidControlEndpoint {
    fn answer(request: &UsbRequest) -> ControlRequestProcessingResult {
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
                1 => Some(DEVICE_DESCRIPTOR.to_vec()),
                2 => Some(CONFIGURATION_DESCRIPTOR.to_vec()),
                3 => string_descriptor(descriptor_index),
                _ => None,
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
        self.response = Some(Self::answer(&request));
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

    fn control(
        request_type: u8,
        request: u8,
        value: u16,
        length: u32,
    ) -> ControlRequestProcessingResult {
        VirtualHidControlEndpoint::answer(&UsbRequest {
            request_type,
            request,
            value,
            length,
            ..Default::default()
        })
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
