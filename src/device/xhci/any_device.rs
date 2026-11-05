//! Type erasure for devices.
//!
//! The controller is generic over one device type, because the endpoint handles of a
//! [`RealDevice`] are associated types with futures that borrow the handle, which makes the traits
//! impossible to use as trait objects. [`AnyDevice`] wraps any [`RealDevice`] behind boxed handles
//! and implements [`RealDevice`] itself, so that passed-through devices of the host, virtual hubs
//! and other emulated devices can sit in the same device tree of one controller.

use std::{fmt::Debug, future::Future, pin::Pin};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{CompleteRealDeviceImpl, DeviceId, RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
        RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
    },
    usbrequest::UsbRequest,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object safe counterpart of [`RealControlEndpointHandle`].
trait DynControlEndpoint: Debug + Send + 'static {
    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<ControlRequestProcessingResult>>;
    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl<T: RealControlEndpointHandle> DynControlEndpoint for T {
    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        RealControlEndpointHandle::submit_control_request(self, request)
    }

    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<ControlRequestProcessingResult>> {
        Box::pin(RealControlEndpointHandle::next_completion(self))
    }

    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(BaseEndpointHandle::cancel(self))
    }
}

/// Object safe counterpart of [`RealInEndpointHandle`].
trait DynInEndpoint: Debug + Send + 'static {
    fn submit(&mut self, data: usize) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<InTrbProcessingResult>>;
    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl<T: RealInEndpointHandle> DynInEndpoint for T {
    fn submit(&mut self, data: usize) -> anyhow::Result<()> {
        RealInEndpointHandle::submit(self, data)
    }

    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<InTrbProcessingResult>> {
        Box::pin(RealInEndpointHandle::next_completion(self))
    }

    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(BaseEndpointHandle::cancel(self))
    }
}

/// Object safe counterpart of [`RealOutEndpointHandle`].
trait DynOutEndpoint: Debug + Send + 'static {
    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<OutTrbProcessingResult>>;
    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl<T: RealOutEndpointHandle> DynOutEndpoint for T {
    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        RealOutEndpointHandle::submit(self, data)
    }

    fn next_completion(&mut self) -> BoxFuture<'_, anyhow::Result<OutTrbProcessingResult>> {
        Box::pin(RealOutEndpointHandle::next_completion(self))
    }

    fn cancel(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(BaseEndpointHandle::cancel(self))
    }
}

/// The control endpoint of an [`AnyDevice`].
#[derive(Debug)]
pub struct AnyControlEndpoint(Box<dyn DynControlEndpoint>);

impl BaseEndpointHandle for AnyControlEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        self.0.cancel()
    }
}

impl RealControlEndpointHandle for AnyControlEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<ControlRequestProcessingResult>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.0.submit_control_request(request)
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        self.0.next_completion()
    }
}

/// An IN endpoint of an [`AnyDevice`].
#[derive(Debug)]
pub struct AnyInEndpoint(Box<dyn DynInEndpoint>);

impl BaseEndpointHandle for AnyInEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        self.0.cancel()
    }
}

impl RealInEndpointHandle for AnyInEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<InTrbProcessingResult>>;

    fn submit(&mut self, data: usize) -> anyhow::Result<()> {
        self.0.submit(data)
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        self.0.next_completion()
    }
}

/// An OUT endpoint of an [`AnyDevice`].
#[derive(Debug)]
pub struct AnyOutEndpoint(Box<dyn DynOutEndpoint>);

impl BaseEndpointHandle for AnyOutEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        self.0.cancel()
    }
}

impl RealOutEndpointHandle for AnyOutEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<OutTrbProcessingResult>>;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        self.0.submit(data)
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        self.0.next_completion()
    }
}

/// Object safe counterpart of [`RealDevice`].
trait DynDevice: Debug + Send + Sync + 'static {
    fn speed(&self) -> Option<Speed>;
    fn control_endpoint_handle(&self) -> AnyControlEndpoint;
    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> AnyInEndpoint;
    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> AnyOutEndpoint;
    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> AnyInEndpoint;
    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> AnyOutEndpoint;
}

impl<D: RealDevice> DynDevice for D {
    fn speed(&self) -> Option<Speed> {
        RealDevice::speed(self)
    }

    fn control_endpoint_handle(&self) -> AnyControlEndpoint {
        AnyControlEndpoint(Box::new(RealDevice::control_endpoint_handle(self)))
    }

    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> AnyInEndpoint {
        AnyInEndpoint(Box::new(RealDevice::bulk_in_endpoint_handle(
            self,
            endpoint_id,
        )))
    }

    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> AnyOutEndpoint {
        AnyOutEndpoint(Box::new(RealDevice::bulk_out_endpoint_handle(
            self,
            endpoint_id,
        )))
    }

    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> AnyInEndpoint {
        AnyInEndpoint(Box::new(RealDevice::interrupt_in_endpoint_handle(
            self,
            endpoint_id,
        )))
    }

    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> AnyOutEndpoint {
        AnyOutEndpoint(Box::new(RealDevice::interrupt_out_endpoint_handle(
            self,
            endpoint_id,
        )))
    }
}

/// The device type of the controller in the `usbvfiod` binary.
pub type ControllerDevice = CompleteRealDeviceImpl<AnyDevice, DeviceId>;

/// Any [`RealDevice`], behind one concrete type.
#[derive(Debug)]
pub struct AnyDevice(Box<dyn DynDevice>);

impl AnyDevice {
    pub fn new<D: RealDevice>(device: D) -> Self {
        Self(Box::new(device))
    }
}

impl RealDevice for AnyDevice {
    type RCEH = AnyControlEndpoint;
    type RBIEH = AnyInEndpoint;
    type RBOEH = AnyOutEndpoint;
    type RIIEH = AnyInEndpoint;
    type RIOEH = AnyOutEndpoint;

    fn speed(&self) -> Option<Speed> {
        self.0.speed()
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        self.0.control_endpoint_handle()
    }

    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
        self.0.bulk_in_endpoint_handle(endpoint_id)
    }

    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
        self.0.bulk_out_endpoint_handle(endpoint_id)
    }

    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
        self.0.interrupt_in_endpoint_handle(endpoint_id)
    }

    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
        self.0.interrupt_out_endpoint_handle(endpoint_id)
    }
}

#[cfg(test)]
mod tests {
    // The glob import would also bring the object safe trait into scope, which makes the methods of
    // `AnyDevice` ambiguous.
    use super::{AnyDevice, BaseEndpointHandle};
    use crate::device::xhci::{
        real_device::{tests::testutils::MockRealDevice, RealDevice, Speed},
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingStatus, OutTrbProcessingStatus,
            RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
        },
        usbrequest::UsbRequest,
    };

    /// A second kind of device, so that the tests mix different types in one collection.
    #[derive(Debug)]
    struct FullSpeedDevice;

    impl RealDevice for FullSpeedDevice {
        type RCEH = <MockRealDevice as RealDevice>::RCEH;
        type RBIEH = <MockRealDevice as RealDevice>::RBIEH;
        type RBOEH = <MockRealDevice as RealDevice>::RBOEH;
        type RIIEH = <MockRealDevice as RealDevice>::RIIEH;
        type RIOEH = <MockRealDevice as RealDevice>::RIOEH;

        fn speed(&self) -> Option<Speed> {
            Some(Speed::Full)
        }

        fn control_endpoint_handle(&self) -> Self::RCEH {
            MockRealDevice::default().control_endpoint_handle()
        }

        fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
            MockRealDevice::default().bulk_in_endpoint_handle(endpoint_id)
        }

        fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
            MockRealDevice::default().bulk_out_endpoint_handle(endpoint_id)
        }

        fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
            MockRealDevice::default().interrupt_in_endpoint_handle(endpoint_id)
        }

        fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
            MockRealDevice::default().interrupt_out_endpoint_handle(endpoint_id)
        }
    }

    #[test]
    fn different_devices_share_one_type_and_keep_their_speed() {
        let devices = [
            AnyDevice::new(MockRealDevice::default()),
            AnyDevice::new(FullSpeedDevice),
            AnyDevice::new(AnyDevice::new(FullSpeedDevice)),
        ];

        let speeds: Vec<_> = devices.iter().map(RealDevice::speed).collect();

        assert_eq!(
            speeds,
            [Some(Speed::Super), Some(Speed::Full), Some(Speed::Full)]
        );
    }

    #[tokio::test]
    async fn in_endpoint_requests_and_completions_reach_the_wrapped_endpoint() {
        let device = AnyDevice::new(MockRealDevice::default());

        let mut endpoint = device.interrupt_in_endpoint_handle(3);
        endpoint.submit(5).unwrap();
        let completion = endpoint.next_completion().await.unwrap();

        assert!(matches!(completion.status, InTrbProcessingStatus::Success));
        assert_eq!(completion.data, [42; 5]);
        endpoint.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn out_endpoint_requests_and_completions_reach_the_wrapped_endpoint() {
        let device = AnyDevice::new(MockRealDevice::default());

        let mut endpoint = device.bulk_out_endpoint_handle(2);
        endpoint.submit(vec![1, 2, 3]).unwrap();
        let completion = endpoint.next_completion().await.unwrap();

        assert!(matches!(completion.status, OutTrbProcessingStatus::Success));
        assert_eq!(completion.length, 3);
        endpoint.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn control_requests_reach_the_wrapped_endpoint() {
        let device = AnyDevice::new(MockRealDevice::default());

        let mut endpoint = device.control_endpoint_handle();
        endpoint
            .submit_control_request(UsbRequest {
                request_type: 0x80,
                request: 6,
                length: 4,
                ..Default::default()
            })
            .unwrap();
        let completion = endpoint.next_completion().await.unwrap();

        assert!(matches!(
            completion,
            ControlRequestProcessingResult::SuccessfulControlIn(data) if data == [42; 4]
        ));
        endpoint.cancel().await.unwrap();
    }
}
