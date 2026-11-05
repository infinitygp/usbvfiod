//! A virtual USB 2.0 hub.
//!
//! The hub answers the standard requests of a device and the hub class requests of chapter 11 of
//! the USB 2.0 specification. Its downstream ports are connected to the devices that the
//! controller attaches below it (see [`HubHandle`]). Transfers to those devices do not go through
//! the hub: the controller routes them directly, so the hub only has to tell the driver where
//! devices are and in which state their ports are.
//!
//! The device and configuration descriptors are generated from the [`HubSpec`], or served verbatim
//! when the spec brings captured ones. The hub class descriptor is always generated: it was never
//! captured from a real hub, so it is a conforming descriptor with these properties: individual
//! port power switching, no compound device, over-current reported per port, a transaction
//! translator think time of 8 full speed bit times, no port indicators, and all devices removable.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, ensure, Context};
use tokio::{sync::watch, time::Instant};
use usbvfiod::hotplug_protocol::{
    hex,
    spec::{HubSpec, UsbSpeed},
};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    hub_ports::{HubHandle, HubShared, Stall},
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
        RealControlEndpointHandle, RealInEndpointHandle,
    },
    stalling_endpoint::{StallingInEndpoint, StallingOutEndpoint},
    usbrequest::UsbRequest,
};

/// The most downstream ports of a hub: the port numbers of a route string have four bits.
pub const MAX_HUB_PORTS: u8 = 15;

/// The device release number if the spec brings none.
const DEFAULT_BCD_DEVICE: u16 = 0x0100;

/// The status change endpoint answers a bitmap that nobody cleared again after this time, which is
/// the polling interval of a real hub. A driver clears the changes right after it read them.
const REPEAT_DELAY: Duration = Duration::from_millis(50);

const DESCRIPTOR_TYPE_DEVICE: u8 = 1;
const DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
const DESCRIPTOR_TYPE_STRING: u8 = 3;
const DESCRIPTOR_TYPE_ENDPOINT: u8 = 5;
const DESCRIPTOR_TYPE_DEVICE_QUALIFIER: u8 = 6;
const DESCRIPTOR_TYPE_OTHER_SPEED_CONFIGURATION: u8 = 7;
const DESCRIPTOR_TYPE_HUB: u8 = 0x29;

const DEVICE_DESCRIPTOR_LENGTH: usize = 18;
const CONFIGURATION_DESCRIPTOR_LENGTH: usize = 9;
const LANGUAGE_ID_ENGLISH_US: u16 = 0x0409;

/// The strings and descriptors that the hub serves.
#[derive(Debug)]
struct Descriptors {
    device: Vec<u8>,
    configuration: Vec<u8>,
    hub: Vec<u8>,
    manufacturer: Option<String>,
    product: Option<String>,
    serial: Option<String>,
}

fn utf16_string_descriptor(text: &str) -> Vec<u8> {
    let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut descriptor = vec![units.len() as u8 + 2, DESCRIPTOR_TYPE_STRING];
    descriptor.extend(units);
    descriptor
}

fn check_string(name: &str, text: Option<&String>) -> anyhow::Result<()> {
    // The length of a string descriptor is one byte.
    if let Some(text) = text {
        ensure!(
            text.encode_utf16().count() <= 126,
            "the {name} string is longer than 126 characters"
        );
    }
    Ok(())
}

/// Split captured bytes into the device descriptor and the configuration descriptor.
fn split_captured_descriptors(raw: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    ensure!(
        raw.len() >= DEVICE_DESCRIPTOR_LENGTH + CONFIGURATION_DESCRIPTOR_LENGTH,
        "the descriptors are {} bytes long, a device descriptor and a configuration descriptor need at least {}",
        raw.len(),
        DEVICE_DESCRIPTOR_LENGTH + CONFIGURATION_DESCRIPTOR_LENGTH
    );
    ensure!(
        raw[0] == DEVICE_DESCRIPTOR_LENGTH as u8 && raw[1] == DESCRIPTOR_TYPE_DEVICE,
        "the descriptors do not start with a device descriptor"
    );
    let (device, rest) = raw.split_at(DEVICE_DESCRIPTOR_LENGTH);
    ensure!(
        rest[0] == CONFIGURATION_DESCRIPTOR_LENGTH as u8
            && rest[1] == DESCRIPTOR_TYPE_CONFIGURATION,
        "a configuration descriptor does not follow the device descriptor"
    );
    let total = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
    ensure!(
        (CONFIGURATION_DESCRIPTOR_LENGTH..=rest.len()).contains(&total),
        "the configuration descriptor announces {total} bytes, but {} follow the device descriptor",
        rest.len()
    );
    Ok((device.to_vec(), rest[..total].to_vec()))
}

impl Descriptors {
    fn new(spec: &HubSpec, speed: Speed) -> anyhow::Result<Self> {
        check_string("manufacturer", spec.manufacturer.as_ref())?;
        check_string("product", spec.product.as_ref())?;
        check_string("serial number", spec.serial.as_ref())?;

        let (device, configuration) = match &spec.descriptors {
            Some(text) => {
                let raw = hex::parse(text).context("the descriptors are not hexadecimal text")?;
                split_captured_descriptors(&raw)?
            }
            None => (
                Self::generate_device_descriptor(spec, speed)?,
                Self::generate_configuration_descriptor(spec.ports, speed),
            ),
        };

        Ok(Self {
            device,
            configuration,
            hub: Self::hub_descriptor(spec.ports),
            manufacturer: spec.manufacturer.clone(),
            product: spec.product.clone(),
            serial: spec.serial.clone(),
        })
    }

    fn generate_device_descriptor(spec: &HubSpec, speed: Speed) -> anyhow::Result<Vec<u8>> {
        let vid = spec.vid.context("the hub needs a vendor ID")?;
        let pid = spec.pid.context("the hub needs a product ID")?;
        let bcd_device = spec.bcd_device.unwrap_or(DEFAULT_BCD_DEVICE);
        let (bcd_usb, protocol) = match speed {
            // A high speed hub with a single transaction translator.
            Speed::High => (0x0200u16, 1),
            _ => (0x0110, 0),
        };
        let string_index = |present: bool, index: u8| if present { index } else { 0 };

        let mut descriptor = vec![DEVICE_DESCRIPTOR_LENGTH as u8, DESCRIPTOR_TYPE_DEVICE];
        descriptor.extend(bcd_usb.to_le_bytes());
        descriptor.extend([9, 0, protocol, 64]); // hub class, no subclass, bMaxPacketSize0
        descriptor.extend(vid.to_le_bytes());
        descriptor.extend(pid.to_le_bytes());
        descriptor.extend(bcd_device.to_le_bytes());
        descriptor.extend([
            string_index(spec.manufacturer.is_some(), 1),
            string_index(spec.product.is_some(), 2),
            string_index(spec.serial.is_some(), 3),
            1, // one configuration
        ]);
        Ok(descriptor)
    }

    /// One configuration with the hub interface and the status change endpoint, which has the
    /// layout of the descriptors captured from the Prolific hub.
    fn generate_configuration_descriptor(ports: u8, speed: Speed) -> Vec<u8> {
        let bitmap_length = usize::from(ports) / 8 + 1;
        // The interval is in frames at full speed and an exponent of microframes at high speed.
        let interval = if speed == Speed::High { 12 } else { 255 };
        let mut descriptor = vec![
            9, 2, 25, 0, 1, 1, 0, 0xe0, 0x32, // configuration: self powered, 100 mA
            9, 4, 0, 0, 1, 9, 0, 0, 0, // the hub interface
            7, 5, 0x81, 3, // interrupt IN endpoint 1
        ];
        descriptor.extend((bitmap_length as u16).to_le_bytes());
        descriptor.push(interval);
        descriptor
    }

    /// The hub descriptor (USB 2.0 specification, table 11-13).
    fn hub_descriptor(ports: u8) -> Vec<u8> {
        // One bit per port and one for the hub, for DeviceRemovable and PortPwrCtrlMask each.
        let mask_length = usize::from(ports) / 8 + 1;
        // Individual power switching, not a compound device, per port over-current protection, a
        // think time of 8 full speed bit times, and no port indicators.
        let characteristics: u16 = 0x0009;
        let mut descriptor = vec![(7 + 2 * mask_length) as u8, DESCRIPTOR_TYPE_HUB, ports];
        descriptor.extend(characteristics.to_le_bytes());
        descriptor.push(50); // 100 ms from the power-on of a port to a stable power
        descriptor.push(50); // the controller of the hub needs 50 mA
        descriptor.extend(vec![0x00; mask_length]); // all devices are removable
        descriptor.extend(vec![0xff; mask_length]); // PortPwrCtrlMask, all ones for compatibility
        descriptor
    }

    /// The string for an index, as a string descriptor. The device descriptor names the indexes.
    fn string(&self, index: u8) -> Option<Vec<u8>> {
        if index == 0 {
            let mut descriptor = vec![4, DESCRIPTOR_TYPE_STRING];
            descriptor.extend(LANGUAGE_ID_ENGLISH_US.to_le_bytes());
            return Some(descriptor);
        }
        [
            (self.device[14], &self.manufacturer),
            (self.device[15], &self.product),
            (self.device[16], &self.serial),
        ]
        .into_iter()
        .find_map(|(string_index, text)| {
            (string_index == index)
                .then_some(text.as_deref())
                .flatten()
                .map(utf16_string_descriptor)
        })
    }

    fn self_powered(&self) -> bool {
        self.configuration[7] & 0x40 != 0
    }

    fn configuration_value(&self) -> u8 {
        self.configuration[5]
    }

    /// The device qualifier of a high speed hub: the device as it looks at full speed.
    fn device_qualifier(&self) -> Vec<u8> {
        let mut descriptor = vec![10, DESCRIPTOR_TYPE_DEVICE_QUALIFIER];
        descriptor.extend([0x00, 0x02]); // bcdUSB 2.00
        descriptor.extend(&self.device[4..6]); // class and subclass
        descriptor.push(0); // a hub works as full speed hub in the other speed
        descriptor.push(self.device[7]);
        descriptor.push(self.device[17]);
        descriptor.push(0);
        descriptor
    }

    /// The configuration of a high speed hub as it looks at full speed.
    fn other_speed_configuration(&self) -> Vec<u8> {
        let mut descriptor = self.configuration.clone();
        descriptor[1] = DESCRIPTOR_TYPE_OTHER_SPEED_CONFIGURATION;
        // The interrupt interval of the endpoints is in frames now.
        let mut offset = 0;
        while offset + 2 <= descriptor.len() {
            let length = usize::from(descriptor[offset]);
            if length < 2 || offset + length > descriptor.len() {
                break;
            }
            if descriptor[offset + 1] == DESCRIPTOR_TYPE_ENDPOINT
                && length >= 7
                && descriptor[offset + 3] & 3 == 3
            {
                descriptor[offset + 6] = 255;
            }
            offset += length;
        }
        descriptor
    }
}

/// What a request to the hub returns.
enum Answer {
    Data(Vec<u8>),
    Done,
}

/// The device side of the hub that its endpoints share.
#[derive(Debug)]
struct HubCore {
    shared: Arc<HubShared>,
    descriptors: Descriptors,
}

impl HubCore {
    fn high_speed(&self) -> bool {
        self.shared.speed() == Speed::High
    }

    fn answer(&self, request: &UsbRequest) -> ControlRequestProcessingResult {
        match self.process(request) {
            Ok(Answer::Data(mut data)) => {
                data.truncate(request.length as usize);
                ControlRequestProcessingResult::SuccessfulControlIn(data)
            }
            Ok(Answer::Done) => ControlRequestProcessingResult::SuccessfulControlOut,
            Err(Stall) => ControlRequestProcessingResult::Stall,
        }
    }

    fn process(&self, request: &UsbRequest) -> Result<Answer, Stall> {
        const STANDARD: u8 = 0;
        const CLASS: u8 = 1;

        let kind = (request.request_type >> 5) & 3;
        match kind {
            STANDARD => self.process_standard(request),
            CLASS => self.process_class(request),
            _ => Err(Stall),
        }
    }

    fn process_standard(&self, request: &UsbRequest) -> Result<Answer, Stall> {
        const GET_STATUS: u8 = 0;
        const CLEAR_FEATURE: u8 = 1;
        const SET_FEATURE: u8 = 3;
        const SET_ADDRESS: u8 = 5;
        const GET_DESCRIPTOR: u8 = 6;
        const GET_CONFIGURATION: u8 = 8;
        const SET_CONFIGURATION: u8 = 9;
        const GET_INTERFACE: u8 = 10;
        const SET_INTERFACE: u8 = 11;
        const FEATURE_ENDPOINT_HALT: u16 = 0;
        const FEATURE_DEVICE_REMOTE_WAKEUP: u16 = 1;

        match (request.request_type, request.request) {
            (0x80, GET_STATUS) => {
                let status = u16::from(self.descriptors.self_powered())
                    | u16::from(self.shared.remote_wakeup()) << 1;
                Ok(Answer::Data(status.to_le_bytes().to_vec()))
            }
            // An interface or an endpoint has nothing but zeros to report.
            (0x81 | 0x82, GET_STATUS) => Ok(Answer::Data(vec![0, 0])),
            (0x00, CLEAR_FEATURE | SET_FEATURE)
                if request.value == FEATURE_DEVICE_REMOTE_WAKEUP =>
            {
                self.shared
                    .set_remote_wakeup(request.request == SET_FEATURE);
                Ok(Answer::Done)
            }
            (0x02, CLEAR_FEATURE | SET_FEATURE) if request.value == FEATURE_ENDPOINT_HALT => {
                Ok(Answer::Done)
            }
            (0x00, SET_ADDRESS) => Ok(Answer::Done),
            (0x80, GET_DESCRIPTOR) => self.get_descriptor(request).map(Answer::Data),
            (0x80, GET_CONFIGURATION) => Ok(Answer::Data(vec![self.shared.configuration()])),
            (0x00, SET_CONFIGURATION) => {
                let configuration = (request.value & 0xff) as u8;
                if configuration == 0 || configuration == self.descriptors.configuration_value() {
                    self.shared.set_configuration(configuration);
                    Ok(Answer::Done)
                } else {
                    Err(Stall)
                }
            }
            (0x81, GET_INTERFACE) if request.index == 0 => Ok(Answer::Data(vec![0])),
            (0x01, SET_INTERFACE) if request.index == 0 && request.value == 0 => Ok(Answer::Done),
            _ => Err(Stall),
        }
    }

    fn get_descriptor(&self, request: &UsbRequest) -> Result<Vec<u8>, Stall> {
        let descriptor_type = (request.value >> 8) as u8;
        let index = (request.value & 0xff) as u8;
        let descriptors = &self.descriptors;

        match (descriptor_type, index) {
            (DESCRIPTOR_TYPE_DEVICE, 0) => Ok(descriptors.device.clone()),
            (DESCRIPTOR_TYPE_CONFIGURATION, 0) => Ok(descriptors.configuration.clone()),
            (DESCRIPTOR_TYPE_STRING, index) => descriptors.string(index).ok_or(Stall),
            // A full speed device does not know about other speeds, and stalls the requests.
            (DESCRIPTOR_TYPE_DEVICE_QUALIFIER, 0) if self.high_speed() => {
                Ok(descriptors.device_qualifier())
            }
            (DESCRIPTOR_TYPE_OTHER_SPEED_CONFIGURATION, 0) if self.high_speed() => {
                Ok(descriptors.other_speed_configuration())
            }
            _ => Err(Stall),
        }
    }

    fn process_class(&self, request: &UsbRequest) -> Result<Answer, Stall> {
        const GET_STATUS: u8 = 0;
        const CLEAR_FEATURE: u8 = 1;
        const SET_FEATURE: u8 = 3;
        const GET_DESCRIPTOR: u8 = 6;
        const CLEAR_TT_BUFFER: u8 = 8;
        const RESET_TT: u8 = 9;
        const GET_TT_STATE: u8 = 10;
        const STOP_TT: u8 = 11;
        const HUB_FEATURE_C_LOCAL_POWER: u16 = 0;
        const HUB_FEATURE_C_OVER_CURRENT: u16 = 1;

        // The port number is in the low byte of wIndex, the high byte holds the selector of the
        // test mode and of the indicators.
        let port = (request.index & 0xff) as u8;

        match (request.request_type, request.request) {
            // The hub has neither local power nor over-current to report.
            (0xa0, GET_STATUS) => Ok(Answer::Data(vec![0; 4])),
            (0xa0, GET_DESCRIPTOR) if (request.value >> 8) as u8 == DESCRIPTOR_TYPE_HUB => {
                Ok(Answer::Data(self.descriptors.hub.clone()))
            }
            (0x20, CLEAR_FEATURE)
                if request.value == HUB_FEATURE_C_LOCAL_POWER
                    || request.value == HUB_FEATURE_C_OVER_CURRENT =>
            {
                Ok(Answer::Done)
            }
            (0xa3, GET_STATUS) => self
                .shared
                .port_status(port)
                .map(|status| Answer::Data(status.to_vec())),
            (0x23, SET_FEATURE) => self
                .shared
                .set_port_feature(port, request.value)
                .map(|()| Answer::Done),
            (0x23, CLEAR_FEATURE) => self
                .shared
                .clear_port_feature(port, request.value)
                .map(|()| Answer::Done),
            // The transaction translator requests exist on high speed hubs. There are no
            // transactions to translate, so there is nothing to clear, reset or stop, and no state.
            (0x23, CLEAR_TT_BUFFER | RESET_TT | STOP_TT) if self.high_speed() => {
                self.check_port(port).map(|()| Answer::Done)
            }
            (0xa3, GET_TT_STATE) if self.high_speed() => {
                self.check_port(port).map(|()| Answer::Data(vec![]))
            }
            _ => Err(Stall),
        }
    }

    fn check_port(&self, port: u8) -> Result<(), Stall> {
        (1..=self.shared.ports())
            .contains(&port)
            .then_some(())
            .ok_or(Stall)
    }
}

/// A virtual USB 2.0 hub.
#[derive(Debug)]
pub struct VirtualHub {
    core: Arc<HubCore>,
}

impl VirtualHub {
    /// Create the hub of a spec, with the handle through which the devices below it connect.
    pub fn new(spec: &HubSpec) -> anyhow::Result<(Self, HubHandle)> {
        ensure!(
            (1..=MAX_HUB_PORTS).contains(&spec.ports),
            "a hub has 1 to {MAX_HUB_PORTS} ports, not {}",
            spec.ports
        );
        let speed = match spec.speed {
            UsbSpeed::Full => Speed::Full,
            UsbSpeed::High => Speed::High,
            other => bail!("a hub is a full speed or a high speed device, not a {other} one"),
        };
        let descriptors = Descriptors::new(spec, speed)?;
        let shared = Arc::new(HubShared::new(spec.ports, speed));
        let handle = HubHandle::new(shared.clone());

        Ok((
            Self {
                core: Arc::new(HubCore {
                    shared,
                    descriptors,
                }),
            },
            handle,
        ))
    }
}

impl RealDevice for VirtualHub {
    type RCEH = HubControlEndpoint;
    type RBIEH = StallingInEndpoint;
    type RBOEH = StallingOutEndpoint;
    type RIIEH = HubStatusChangeEndpoint;
    type RIOEH = StallingOutEndpoint;

    fn speed(&self) -> Option<Speed> {
        Some(self.core.shared.speed())
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        HubControlEndpoint {
            core: self.core.clone(),
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
        HubStatusChangeEndpoint {
            changes: self.core.shared.subscribe(),
            core: self.core.clone(),
            requested: 0,
            last_reported: None,
        }
    }

    fn interrupt_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIOEH {
        StallingOutEndpoint
    }
}

/// The default control endpoint of the hub.
#[derive(Debug)]
pub struct HubControlEndpoint {
    core: Arc<HubCore>,
    response: Option<ControlRequestProcessingResult>,
}

impl RealControlEndpointHandle for HubControlEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.response = Some(self.core.answer(&request));
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            self.response
                .take()
                .ok_or_else(|| anyhow!("no control request was submitted"))
        })
    }
}

impl BaseEndpointHandle for HubControlEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// The interrupt IN endpoint that reports the ports with a status change.
///
/// A transfer completes with the bitmap when a port has a change, and stays pending as long as
/// there is none, like a transfer that a real hub answers with NAK.
#[derive(Debug)]
pub struct HubStatusChangeEndpoint {
    core: Arc<HubCore>,
    changes: watch::Receiver<()>,
    requested: usize,
    /// What the last completed transfer returned, and when.
    last_reported: Option<(Vec<u8>, Instant)>,
}

impl RealInEndpointHandle for HubStatusChangeEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, data: usize) -> anyhow::Result<()> {
        self.requested = data;
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            loop {
                if let Some(mut bitmap) = self.core.shared.status_change_bitmap() {
                    // The driver clears a change after it read it. A bitmap that is still the same
                    // after a poll interval is reported again, but not in a busy loop.
                    let wait = self
                        .last_reported
                        .as_ref()
                        .filter(|(last, _)| *last == bitmap)
                        .and_then(|(_, when)| {
                            (*when + REPEAT_DELAY).checked_duration_since(Instant::now())
                        });
                    if let Some(wait) = wait {
                        tokio::time::sleep(wait).await;
                        continue;
                    }

                    self.last_reported = Some((bitmap.clone(), Instant::now()));
                    bitmap.truncate(self.requested);
                    return Ok(InTrbProcessingResult {
                        status: InTrbProcessingStatus::Success,
                        data: bitmap,
                    });
                }

                // The change cannot slip through between the check and the wait: the receiver
                // remembers changes that happened since it last looked.
                self.changes
                    .changed()
                    .await
                    .map_err(|_| anyhow!("the hub is gone"))?;
            }
        })
    }
}

impl BaseEndpointHandle for HubStatusChangeEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;
    use crate::device::xhci::hub_ports::{port_change, port_feature, port_status};

    /// The descriptors captured from the Prolific hub (class HW): device descriptor, then
    /// configuration descriptor.
    const CAPTURED: &str = "
        12 01 00 02 09 00 01 40 7b 06 86 25 00 00 01 03
        00 01 09 02 19 00 01 01 00 e0 32 09 04 00 00 01
        09 00 00 00 07 05 81 03 01 00 0c";

    const GET_STATUS: u8 = 0;
    const CLEAR_FEATURE: u8 = 1;
    const SET_FEATURE: u8 = 3;
    const GET_DESCRIPTOR: u8 = 6;

    fn spec(ports: u8, speed: UsbSpeed) -> HubSpec {
        HubSpec {
            vid: Some(0x067b),
            pid: Some(0x2586),
            bcd_device: Some(0),
            manufacturer: Some("USB Device".to_string()),
            product: Some("USB 2.0 Hub".to_string()),
            serial: None,
            ports,
            speed,
            descriptors: None,
        }
    }

    fn captured_spec() -> HubSpec {
        HubSpec {
            vid: None,
            pid: None,
            bcd_device: None,
            // The strings as the kernel read them from the real hub, with the padding.
            manufacturer: Some("USB Device  ".to_string()),
            product: Some("USB 2.0 Hub            ".to_string()),
            serial: None,
            ports: 4,
            speed: UsbSpeed::High,
            descriptors: Some(CAPTURED.to_string()),
        }
    }

    fn hub(spec: &HubSpec) -> (VirtualHub, HubHandle) {
        VirtualHub::new(spec).unwrap()
    }

    fn request(request_type: u8, request: u8, value: u16, index: u16, length: u32) -> UsbRequest {
        UsbRequest {
            request_type,
            request,
            value,
            index,
            length,
            ..Default::default()
        }
    }

    fn control(
        hub: &VirtualHub,
        request_type: u8,
        request_code: u8,
        value: u16,
        index: u16,
        length: u32,
    ) -> ControlRequestProcessingResult {
        hub.core
            .answer(&request(request_type, request_code, value, index, length))
    }

    fn data(result: ControlRequestProcessingResult) -> Vec<u8> {
        match result {
            ControlRequestProcessingResult::SuccessfulControlIn(data) => data,
            other => panic!("unexpected result {other:?}"),
        }
    }

    fn assert_done(result: ControlRequestProcessingResult) {
        assert!(
            matches!(result, ControlRequestProcessingResult::SuccessfulControlOut),
            "{result:?}"
        );
    }

    fn assert_stall(result: ControlRequestProcessingResult) {
        assert!(
            matches!(result, ControlRequestProcessingResult::Stall),
            "{result:?}"
        );
    }

    fn port_status_of(hub: &VirtualHub, port: u16) -> (u16, u16) {
        let data = data(control(hub, 0xa3, GET_STATUS, 0, port, 4));
        assert_eq!(data.len(), 4);
        (
            u16::from_le_bytes([data[0], data[1]]),
            u16::from_le_bytes([data[2], data[3]]),
        )
    }

    fn set_port_feature(
        hub: &VirtualHub,
        port: u16,
        feature: u16,
    ) -> ControlRequestProcessingResult {
        control(hub, 0x23, SET_FEATURE, feature, port, 0)
    }

    fn clear_port_feature(
        hub: &VirtualHub,
        port: u16,
        feature: u16,
    ) -> ControlRequestProcessingResult {
        control(hub, 0x23, CLEAR_FEATURE, feature, port, 0)
    }

    #[test]
    fn captured_descriptors_are_served_verbatim() {
        let (hub, _) = hub(&captured_spec());
        let captured = hex::parse(CAPTURED).unwrap();

        assert_eq!(
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0100, 0, 64)),
            captured[..18]
        );
        assert_eq!(
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0200, 0, 255)),
            captured[18..]
        );
        // The driver reads the first bytes before it knows the length.
        assert_eq!(
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0100, 0, 8)),
            captured[..8]
        );
        assert_eq!(
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0200, 0, 9)),
            captured[18..27]
        );
    }

    #[test]
    fn strings_are_served_for_the_indexes_the_device_descriptor_names() {
        let (hub, _) = hub(&captured_spec());

        assert_eq!(
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0300, 0, 255)),
            [4, 3, 0x09, 0x04]
        );
        // The captured device descriptor has the manufacturer at index 1 and the product at 3.
        let manufacturer = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0301, 0x0409, 255));
        assert_eq!(usize::from(manufacturer[0]), manufacturer.len());
        assert_eq!(manufacturer.len(), 2 + 2 * "USB Device  ".len());
        assert_eq!(
            String::from_utf16(
                &manufacturer[2..]
                    .chunks(2)
                    .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                    .collect::<Vec<_>>()
            )
            .unwrap(),
            "USB Device  "
        );
        let product = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0303, 0x0409, 255));
        assert_eq!(product.len(), 2 + 2 * "USB 2.0 Hub            ".len());
        // There is no string at the index in between, and no serial number.
        assert_stall(control(&hub, 0x80, GET_DESCRIPTOR, 0x0302, 0x0409, 255));
        assert_stall(control(&hub, 0x80, GET_DESCRIPTOR, 0x03ee, 0, 255));
    }

    #[test]
    fn a_generated_descriptor_set_matches_the_capture_for_four_ports() {
        let (hub, _) = hub(&spec(4, UsbSpeed::High));
        let captured = hex::parse(CAPTURED).unwrap();

        let device = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0100, 0, 255));
        let configuration = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0200, 0, 255));

        assert_eq!(configuration, captured[18..]);
        // Only the indexes of the strings differ: the real hub names its product at index 3.
        assert_eq!(device[..14], captured[..14]);
        assert_eq!(device[17], captured[17]);
        assert_eq!(device[14..17], [1, 2, 0]);
        assert_eq!(
            usize::from(u16::from_le_bytes([configuration[2], configuration[3]])),
            configuration.len()
        );
    }

    #[test]
    fn a_full_speed_hub_is_a_usb_1_1_device_without_the_other_speed_descriptors() {
        let (hub, _) = hub(&spec(4, UsbSpeed::Full));

        let device = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0100, 0, 255));
        let configuration = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0200, 0, 255));

        assert_eq!(device[2..4], [0x10, 0x01], "bcdUSB 1.10");
        assert_eq!(
            device[4..7],
            [9, 0, 0],
            "a hub without transaction translator"
        );
        assert_eq!(
            configuration[configuration.len() - 1],
            255,
            "bInterval in frames"
        );
        assert_stall(control(&hub, 0x80, GET_DESCRIPTOR, 0x0600, 0, 10));
        assert_stall(control(&hub, 0x80, GET_DESCRIPTOR, 0x0700, 0, 255));
    }

    #[test]
    fn a_high_speed_hub_describes_itself_at_full_speed_too() {
        let (hub, _) = hub(&spec(4, UsbSpeed::High));

        let qualifier = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0600, 0, 255));
        let other_speed = data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0700, 0, 255));

        assert_eq!(qualifier, [10, 6, 0x00, 0x02, 9, 0, 0, 64, 1, 0]);
        assert_eq!(other_speed.len(), 25);
        assert_eq!(other_speed[1], 7);
        assert_eq!(
            other_speed[2..24],
            data(control(&hub, 0x80, GET_DESCRIPTOR, 0x0200, 0, 255))[2..24]
        );
        assert_eq!(
            other_speed[24], 255,
            "the interval of the endpoint in frames"
        );
    }

    #[test]
    fn the_hub_descriptor_follows_the_port_count() {
        let (hub4, _) = hub(&spec(4, UsbSpeed::High));
        let (hub15, _) = hub(&spec(15, UsbSpeed::High));

        let descriptor4 = data(control(&hub4, 0xa0, GET_DESCRIPTOR, 0x2900, 0, 255));
        let descriptor15 = data(control(&hub15, 0xa0, GET_DESCRIPTOR, 0x2900, 0, 255));

        assert_eq!(
            descriptor4,
            [9, 0x29, 4, 0x09, 0x00, 50, 50, 0x00, 0xff],
            "individual power switching, per port over-current, 8 bit times think time"
        );
        assert_eq!(descriptor15.len(), 11);
        assert_eq!(descriptor15[0], 11);
        assert_eq!(descriptor15[2], 15);
        assert_eq!(descriptor15[7..9], [0, 0], "DeviceRemovable");
        assert_eq!(descriptor15[9..], [0xff, 0xff], "PortPwrCtrlMask");
        // The driver reads the fixed part first.
        assert_eq!(
            data(control(&hub15, 0xa0, GET_DESCRIPTOR, 0x2900, 0, 7)),
            descriptor15[..7]
        );
    }

    #[test]
    fn the_spec_is_checked() {
        for ports in [0, 16, 255] {
            VirtualHub::new(&spec(ports, UsbSpeed::High)).unwrap_err();
        }
        VirtualHub::new(&spec(4, UsbSpeed::Super)).unwrap_err();
        VirtualHub::new(&spec(4, UsbSpeed::Low)).unwrap_err();
        let mut no_identity = spec(4, UsbSpeed::High);
        no_identity.vid = None;
        VirtualHub::new(&no_identity).unwrap_err();
        let mut long_string = spec(4, UsbSpeed::High);
        long_string.product = Some("x".repeat(127));
        VirtualHub::new(&long_string).unwrap_err();
        for descriptors in [
            "",
            "12 01",
            "zz",
            &CAPTURED[..CAPTURED.len() - 12],
            "13 01 00",
        ] {
            let mut bad = captured_spec();
            bad.descriptors = Some(descriptors.to_string());
            VirtualHub::new(&bad).unwrap_err();
        }
        // A configuration descriptor that announces more bytes than there are.
        let mut bad = captured_spec();
        bad.descriptors = Some(CAPTURED.replace("19 00 01", "30 00 01"));
        VirtualHub::new(&bad).unwrap_err();
    }

    #[test]
    fn standard_requests_of_the_driver() {
        let (hub, _) = hub(&captured_spec());

        // Self powered, no remote wakeup yet.
        assert_eq!(data(control(&hub, 0x80, GET_STATUS, 0, 0, 2)), [1, 0]);
        assert_done(control(&hub, 0x00, SET_FEATURE, 1, 0, 0));
        assert_eq!(data(control(&hub, 0x80, GET_STATUS, 0, 0, 2)), [3, 0]);
        assert_done(control(&hub, 0x00, CLEAR_FEATURE, 1, 0, 0));
        assert_eq!(data(control(&hub, 0x80, GET_STATUS, 0, 0, 2)), [1, 0]);
        assert_eq!(data(control(&hub, 0x81, GET_STATUS, 0, 0, 2)), [0, 0]);
        assert_eq!(data(control(&hub, 0x82, GET_STATUS, 0, 0x81, 2)), [0, 0]);

        assert_eq!(data(control(&hub, 0x80, 8, 0, 0, 1)), [0]);
        assert_done(control(&hub, 0x00, 9, 1, 0, 0));
        assert_eq!(data(control(&hub, 0x80, 8, 0, 0, 1)), [1]);
        assert_stall(control(&hub, 0x00, 9, 2, 0, 0));
        assert_eq!(data(control(&hub, 0x80, 8, 0, 0, 1)), [1]);
        assert_done(control(&hub, 0x00, 9, 0, 0, 0));
        assert_eq!(data(control(&hub, 0x80, 8, 0, 0, 1)), [0]);

        assert_eq!(data(control(&hub, 0x81, 10, 0, 0, 1)), [0]);
        assert_done(control(&hub, 0x01, 11, 0, 0, 0));
        assert_stall(control(&hub, 0x01, 11, 1, 0, 0));
        assert_stall(control(&hub, 0x81, 10, 0, 1, 1));

        assert_done(control(&hub, 0x02, CLEAR_FEATURE, 0, 0x81, 0));
        assert_done(control(&hub, 0x00, 5, 7, 0, 0));
        // Test mode and everything else that does not exist.
        assert_stall(control(&hub, 0x00, SET_FEATURE, 2, 0x0400, 0));
        assert_stall(control(&hub, 0x00, 7, 0, 0, 0));
        assert_stall(control(&hub, 0x40, 0x42, 0, 0, 0));
        assert_stall(control(&hub, 0xc0, 0x42, 0, 0, 0));
        assert_stall(control(&hub, 0x80, GET_DESCRIPTOR, 0x0f00, 0, 255));
    }

    #[test]
    fn hub_class_requests_for_the_hub() {
        let (hub, _) = hub(&captured_spec());

        assert_eq!(data(control(&hub, 0xa0, GET_STATUS, 0, 0, 4)), [0; 4]);
        assert_done(control(&hub, 0x20, CLEAR_FEATURE, 0, 0, 0));
        assert_done(control(&hub, 0x20, CLEAR_FEATURE, 1, 0, 0));
        assert_stall(control(&hub, 0x20, CLEAR_FEATURE, 2, 0, 0));
        assert_stall(control(&hub, 0x20, SET_FEATURE, 0, 0, 0));
        assert_stall(control(&hub, 0x20, 7, 0x2900, 0, 9));
    }

    #[test]
    fn the_driver_connects_a_device_through_the_port_requests() {
        let (hub, handle) = hub(&captured_spec());
        assert_eq!(port_status_of(&hub, 2), (0, 0));

        // The driver powers the ports, and the device below one of them appears.
        for port in 1..=4 {
            assert_done(set_port_feature(&hub, port, port_feature::POWER));
        }
        handle.connect(2, Speed::Full, None);
        assert_eq!(
            port_status_of(&hub, 2),
            (
                port_status::POWER | port_status::CONNECTION,
                port_change::CONNECTION
            )
        );
        assert_done(clear_port_feature(&hub, 2, port_feature::C_CONNECTION));

        // It resets the port and reads the speed.
        assert_done(set_port_feature(&hub, 2, port_feature::RESET));
        assert_eq!(
            port_status_of(&hub, 2),
            (
                port_status::POWER | port_status::CONNECTION | port_status::ENABLE,
                port_change::RESET
            )
        );
        assert_done(clear_port_feature(&hub, 2, port_feature::C_RESET));
        assert_eq!(port_status_of(&hub, 2).1, 0);

        handle.disconnect(2);
        assert_eq!(
            port_status_of(&hub, 2),
            (port_status::POWER, port_change::CONNECTION)
        );
    }

    #[test]
    fn port_requests_to_ports_and_features_that_do_not_exist_stall() {
        let (hub, _) = hub(&captured_spec());

        for port in [0, 5, 255, 0x0100] {
            assert_stall(control(&hub, 0xa3, GET_STATUS, 0, port, 4));
            assert_stall(set_port_feature(&hub, port, port_feature::POWER));
        }
        // PORT_TEST and PORT_INDICATOR, with the selector in the high byte of wIndex.
        assert_stall(control(&hub, 0x23, SET_FEATURE, 21, 0x0401, 0));
        assert_stall(control(&hub, 0x23, SET_FEATURE, 22, 0x0101, 0));
        assert_stall(control(&hub, 0x23, CLEAR_FEATURE, 22, 0x0001, 0));
        assert_stall(clear_port_feature(&hub, 1, 99));
        // Whatever the direction bit says, a request type that is no hub request stalls.
        assert_stall(control(&hub, 0x21, SET_FEATURE, 8, 1, 0));
        assert_stall(control(&hub, 0xa1, GET_STATUS, 0, 1, 4));
    }

    #[test]
    fn transaction_translator_requests_are_accepted_by_a_high_speed_hub() {
        let (hub, _) = hub(&captured_spec());

        for request_code in [8, 9, 11] {
            assert_done(control(&hub, 0x23, request_code, 0x0102, 1, 0));
        }
        assert_eq!(data(control(&hub, 0xa3, 10, 0, 1, 8)), Vec::<u8>::new());
        assert_stall(control(&hub, 0x23, 9, 0, 0, 0));
        assert_stall(control(&hub, 0xa3, 10, 0, 5, 8));

        let (full, _) = self::hub(&spec(4, UsbSpeed::Full));
        for request_code in [8, 9, 11] {
            assert_stall(control(&full, 0x23, request_code, 0, 1, 0));
        }
        assert_stall(control(&full, 0xa3, 10, 0, 1, 8));
    }

    #[test]
    fn the_hub_is_reset_when_a_reset_reaches_it() {
        let (hub, handle) = hub(&captured_spec());
        assert_done(control(&hub, 0x00, 9, 1, 0, 0));
        assert_done(set_port_feature(&hub, 1, port_feature::POWER));
        handle.connect(1, Speed::Full, None);

        handle.bus_reset();

        assert_eq!(data(control(&hub, 0x80, 8, 0, 0, 1)), [0]);
        assert_eq!(port_status_of(&hub, 1), (0, 0));
    }

    #[tokio::test]
    async fn the_status_change_endpoint_stays_pending_until_a_port_changes() {
        let (hub, handle) = hub(&captured_spec());
        let mut endpoint = hub.interrupt_in_endpoint_handle(3);
        endpoint.submit(2).unwrap();

        // Nothing to report: the transfer is not answered.
        timeout(Duration::from_millis(100), endpoint.next_completion())
            .await
            .unwrap_err();

        // The driver powers the port, and a device appears.
        assert_done(set_port_feature(&hub, 3, port_feature::POWER));
        let waiting = tokio::spawn(async move {
            let completion = endpoint.next_completion().await.unwrap();
            (endpoint, completion)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.connect(3, Speed::High, None);

        let (mut endpoint, completion) = timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(completion.status, InTrbProcessingStatus::Success));
        assert_eq!(
            completion.data,
            [1 << 3],
            "port 3, cut to the requested length"
        );

        // The driver reads the change and clears it, which ends the reports.
        assert_done(clear_port_feature(&hub, 3, port_feature::C_CONNECTION));
        endpoint.submit(2).unwrap();
        timeout(Duration::from_millis(100), endpoint.next_completion())
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn the_status_change_endpoint_reports_a_change_that_is_still_there_again_after_a_delay() {
        let (hub, handle) = hub(&captured_spec());
        assert_done(set_port_feature(&hub, 1, port_feature::POWER));
        handle.connect(1, Speed::High, None);
        let mut endpoint = hub.interrupt_in_endpoint_handle(3);
        endpoint.submit(1).unwrap();

        let start = Instant::now();
        let first = endpoint.next_completion().await.unwrap();
        assert!(
            start.elapsed() < REPEAT_DELAY,
            "the first report is immediate"
        );
        endpoint.submit(1).unwrap();
        let second = endpoint.next_completion().await.unwrap();

        assert_eq!(first.data, [1 << 1]);
        assert_eq!(second.data, [1 << 1]);
        assert!(start.elapsed() >= REPEAT_DELAY, "no busy loop of reports");
    }

    #[tokio::test]
    async fn the_other_endpoints_stall() {
        let (hub, _) = hub(&captured_spec());

        let mut bulk_in = hub.bulk_in_endpoint_handle(2);
        bulk_in.submit(8).unwrap();
        assert!(matches!(
            bulk_in.next_completion().await.unwrap().status,
            InTrbProcessingStatus::Stall
        ));
    }
}
