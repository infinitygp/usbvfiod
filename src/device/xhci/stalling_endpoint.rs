//! Endpoints of emulated devices that do not exist on the device.
//!
//! A driver does not configure an endpoint that the descriptors do not announce, but the controller
//! needs a handle for every type of endpoint. A transfer to one of these endpoints stalls, like on
//! a device that has no such endpoint.

use std::{future::Future, pin::Pin};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_endpoint_handle::{
        InTrbProcessingResult, InTrbProcessingStatus, OutTrbProcessingResult,
        OutTrbProcessingStatus, RealInEndpointHandle, RealOutEndpointHandle,
    },
};

/// An IN endpoint that stalls every transfer.
#[derive(Debug, Default)]
pub struct StallingInEndpoint;

impl RealInEndpointHandle for StallingInEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, _data: usize) -> anyhow::Result<()> {
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            Ok(InTrbProcessingResult {
                status: InTrbProcessingStatus::Stall,
                data: vec![],
            })
        })
    }
}

impl BaseEndpointHandle for StallingInEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// An OUT endpoint that stalls every transfer.
#[derive(Debug, Default)]
pub struct StallingOutEndpoint;

impl RealOutEndpointHandle for StallingOutEndpoint {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<OutTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, _data: Vec<u8>) -> anyhow::Result<()> {
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            Ok(OutTrbProcessingResult {
                status: OutTrbProcessingStatus::Stall,
                length: 0,
            })
        })
    }
}

impl BaseEndpointHandle for StallingOutEndpoint {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}
