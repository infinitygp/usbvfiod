use tokio_util::sync::CancellationToken;
use usbvfiod::hotplug_protocol::spec::UsbSpeed;

use crate::device::xhci::{
    hub_ports::HubHandle,
    real_endpoint_handle::{
        RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
    },
};

use std::fmt::{self, Debug};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
    SuperPlus = 5,
}

impl Speed {
    pub const fn is_usb2_speed(self) -> bool {
        self as u8 <= 3
    }
}

impl From<Speed> for UsbSpeed {
    fn from(speed: Speed) -> Self {
        match speed {
            Speed::Low => Self::Low,
            Speed::Full => Self::Full,
            Speed::High => Self::High,
            Speed::Super => Self::Super,
            Speed::SuperPlus => Self::SuperPlus,
        }
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Low => "Low Speed (1.5 Mbps)",
            Self::Full => "Full Speed (12 Mbps)",
            Self::High => "High Speed (480 Mbps)",
            Self::Super => "SuperSpeed (5 Gbps)",
            Self::SuperPlus => "SuperSpeed+ (10/20 Gbps)",
        };
        write!(f, "{name}")
    }
}

pub trait RealDevice: Debug + Send + Sync + 'static {
    type RCEH: RealControlEndpointHandle;
    type RBIEH: RealInEndpointHandle;
    type RBOEH: RealOutEndpointHandle;
    type RIIEH: RealInEndpointHandle;
    type RIOEH: RealOutEndpointHandle;

    fn speed(&self) -> Option<Speed>;
    fn control_endpoint_handle(&self) -> Self::RCEH;
    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH;
    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH;
    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH;
    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH;
}

pub trait Identifier: Debug + Copy + Eq + Send + Sync + 'static {}

impl Identifier for (u8, u8) {}

/// The identifier of devices in a controller that serves devices of the host and emulated ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceId {
    /// A device of the host, by bus and device number.
    Host { bus: u8, device: u8 },
    /// An emulated device. The number is unique within the process.
    Virtual(u32),
}

impl Identifier for DeviceId {}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host { bus, device } => write!(f, "host {bus:03}:{device:03}"),
            Self::Virtual(number) => write!(f, "virtual {number}"),
        }
    }
}

// A RealDevice trait coupled with an identifier and cancellation token for detach notification.
//
// A real device alone might not be able to identify itself. For example, a
// nusb device can only query information from the device; if the device has
// no unique serial number, then fields such as vendor id and product id are
// the best bet for identification. However, with two identical devices, the
// approach fails to uniquely identify the devices. Complete real device allows
// distinction of devices by storing an externally-decided, unique identifier.
// Host bus-/device-number is an option, but so are randomly chosen (but unique)
// numbers or strings.
//
// A CompleteRealDevice must also provide a CancellationToken that can be used
// by the potentially multiple references to the real device (endpoint handles
// must have some kind of reference to talk to the endpoint of the real device)
// to synchronously drop to release the real device upon detach.
pub trait CompleteRealDevice: Debug + Send + Sync + 'static {
    type RD: RealDevice;
    type ID: Identifier;

    fn identifier(&self) -> Self::ID;
    fn realdevice_ref(&self) -> &Self::RD;
    fn detach_token(&self) -> CancellationToken;

    /// The ports of the device, if it is a hub that devices can be attached below.
    fn hub(&self) -> Option<HubHandle> {
        None
    }
}

#[derive(Debug)]
pub struct CompleteRealDeviceImpl<RD: RealDevice, ID: Identifier> {
    pub identifier: ID,
    pub real_device: RD,
    pub cancel: CancellationToken,
    pub hub: Option<HubHandle>,
}

impl<RD: RealDevice, ID: Identifier> CompleteRealDeviceImpl<RD, ID> {
    pub fn new(identifier: ID, real_device: RD) -> Self {
        Self {
            identifier,
            real_device,
            cancel: CancellationToken::new(),
            hub: None,
        }
    }

    /// The device is a hub with these ports.
    #[must_use]
    pub fn with_hub(mut self, hub: HubHandle) -> Self {
        self.hub = Some(hub);
        self
    }
}

impl<RD: RealDevice, ID: Identifier> CompleteRealDevice for CompleteRealDeviceImpl<RD, ID> {
    type RD = RD;
    type ID = ID;

    fn identifier(&self) -> Self::ID {
        self.identifier
    }

    fn realdevice_ref(&self) -> &Self::RD {
        &self.real_device
    }

    fn detach_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn hub(&self) -> Option<HubHandle> {
        self.hub.clone()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub mod testutils {
        use super::*;

        use crate::device::xhci::{
            endpoint_handle::tests::testutils::{
                MockRealControlEndpointReadStatic, MockRealInEndpoint, MockRealOutEndpoint,
            },
            real_device,
        };

        #[derive(Default, Debug)]
        pub struct MockRealDevice {}

        /// A device with endpoints like the ones of [`MockRealDevice`] and a speed of choice.
        #[derive(Debug)]
        pub struct MockRealDeviceWithSpeed(pub Speed);

        impl From<Speed> for MockRealDeviceWithSpeed {
            fn from(speed: Speed) -> Self {
                Self(speed)
            }
        }

        impl RealDevice for MockRealDeviceWithSpeed {
            type RCEH = MockRealControlEndpointReadStatic;
            type RBIEH = MockRealInEndpoint;
            type RBOEH = MockRealOutEndpoint;
            type RIIEH = MockRealInEndpoint;
            type RIOEH = MockRealOutEndpoint;

            fn speed(&self) -> Option<real_device::Speed> {
                Some(self.0)
            }

            fn control_endpoint_handle(&self) -> Self::RCEH {
                MockRealControlEndpointReadStatic::new(None)
            }

            fn bulk_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBIEH {
                MockRealInEndpoint::new(None)
            }

            fn bulk_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBOEH {
                MockRealOutEndpoint::new()
            }

            fn interrupt_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIIEH {
                MockRealInEndpoint::new(None)
            }

            fn interrupt_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIOEH {
                MockRealOutEndpoint::new()
            }
        }

        impl RealDevice for MockRealDevice {
            type RCEH = MockRealControlEndpointReadStatic;
            type RBIEH = MockRealInEndpoint;
            type RBOEH = MockRealOutEndpoint;
            type RIIEH = MockRealInEndpoint;
            type RIOEH = MockRealOutEndpoint;

            fn speed(&self) -> Option<real_device::Speed> {
                Some(Speed::Super)
            }

            fn control_endpoint_handle(&self) -> Self::RCEH {
                MockRealControlEndpointReadStatic::new(None)
            }

            fn bulk_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBIEH {
                MockRealInEndpoint::new(None)
            }

            fn bulk_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RBOEH {
                MockRealOutEndpoint::new()
            }

            fn interrupt_in_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIIEH {
                MockRealInEndpoint::new(None)
            }

            fn interrupt_out_endpoint_handle(&self, _endpoint_id: u8) -> Self::RIOEH {
                MockRealOutEndpoint::new()
            }
        }
    }
}
