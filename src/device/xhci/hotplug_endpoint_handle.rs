use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc};

use tokio::{runtime, select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::device::{
    pci::constants::xhci::rings::trb_types,
    xhci::{
        endpoint_handle::{DummyEndpointHandle, EndpointHandle, TrbProcessingResult},
        interrupter::EventSender,
        trb::{CompletionCode, EventTrb, RawTrb},
    },
};

// same as TrbProcessingResult but without the Disconnect variant.
// The endpoint state machine should not know about real-device disconnects;
// TRBs to detached device should look like TransactionErrors to the endpoint
// state machine.
#[derive(Debug, Clone, Copy)]
pub enum HotplugTrbProcessingResult {
    Ok,
    Stall(Option<(u64, bool)>),
    TrbError,
    TransactionError(Option<(u64, bool)>),
}

impl HotplugTrbProcessingResult {
    const fn map_result(value: TrbProcessingResult) -> Self {
        match value {
            TrbProcessingResult::Ok => Self::Ok,
            TrbProcessingResult::Stall(addr) => Self::Stall(addr),
            TrbProcessingResult::TrbError => Self::TrbError,
            TrbProcessingResult::TransactionError(addr) => Self::TransactionError(addr),
            // A device disconnect looks like a failed transaction for the endpoint state machine
            TrbProcessingResult::Disconnect => Self::TransactionError(None),
        }
    }
}

// all traits in the endpoint abstractions implement this trait
pub trait BaseEndpointHandle: Debug + Send + 'static {
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
}

// trait exists to hide the EndpointHandle generic parameter from the endpoint state machine
pub trait HotplugEndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<HotplugTrbProcessingResult>>
        + Send
        + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;

    /// Where the transfer ring continues after [`BaseEndpointHandle::cancel`]: the address and
    /// cycle state of the first TRB of the transfer that the cancel abandoned, if there was one.
    ///
    /// The endpoint handle works on a whole TD at a time, so an endpoint that stops does not
    /// continue with the TRB it stopped at, but with the first TRB of its TD.
    fn take_restart_position(&mut self) -> Option<(u64, bool)>;
}

#[derive(Debug)]
pub struct HotplugEndpointHandleImpl<EH: EndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    endpoint_handle: Arc<Mutex<Option<EH>>>,
    event_sender: EventSender,
    notify_detach: CancellationToken,
    notify_drop: CancellationToken,
    submission_state: HotplugSubmissionState,
    /// The first TRB of the transfer that the endpoint handle works on, which can be several TRBs
    /// before the handle talks to the device.
    transfer_start: Option<RawTrb>,
    /// See [`HotplugEndpointHandle::take_restart_position`].
    restart_position: Option<(u64, bool)>,
}

#[derive(Debug, Default)]
enum HotplugSubmissionState {
    #[default]
    NoTrbSubmitted,
    TrbSubmitted {
        address: u64,
    },
}

/// The Transfer Event of an endpoint that stopped in the transfer that starts with this TRB.
///
/// The endpoint handle did not start the transfer on the device as long as it collected the
/// TRBs, and starts it again from the first TRB, so no byte of the TRB is transferred. The event
/// names the TRB with its full length as the residual. The length of a TRB that does not transfer
/// data is not valid.
const fn stopped_event(first_trb: &RawTrb, endpoint_id: u8, slot_id: u8) -> EventTrb {
    let (completion_code, residual_length) = match first_trb.buffer[13] >> 2 {
        trb_types::NORMAL | trb_types::SETUP_STAGE | trb_types::DATA_STAGE | trb_types::ISOCH => {
            // Bits 0 to 16 of the third dword of a transfer TRB.
            let length = u32::from_le_bytes([
                first_trb.buffer[8],
                first_trb.buffer[9],
                first_trb.buffer[10],
                first_trb.buffer[11],
            ]) & 0x1_ffff;
            (CompletionCode::Stopped, length)
        }
        _ => (CompletionCode::StoppedLengthInvalid, 0),
    };
    EventTrb::new_transfer_event_trb(
        first_trb.address,
        residual_length,
        completion_code,
        false,
        endpoint_id,
        slot_id,
    )
}

impl<EH: EndpointHandle> HotplugEndpointHandleImpl<EH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        endpoint_handle: EH,
        event_sender: EventSender,
        notify_detach: CancellationToken,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let endpoint_handle = Arc::new(Mutex::new(Some(endpoint_handle)));
        let notify_drop = CancellationToken::new();

        async_runtime.spawn(Self::detach_handler(
            endpoint_handle.clone(),
            notify_detach.clone(),
            notify_drop.clone(),
        ));

        Self {
            slot_id,
            endpoint_id,
            endpoint_handle,
            event_sender,
            notify_detach,
            notify_drop,
            submission_state: HotplugSubmissionState::NoTrbSubmitted,
            transfer_start: None,
            restart_position: None,
        }
    }

    // wait for signal of other endpoints or the central detach.
    // Drop device when receiving notification.
    async fn detach_handler(
        endpoint_handle: Arc<Mutex<Option<EH>>>,
        notify_detach: CancellationToken,
        notify_drop: CancellationToken,
    ) {
        select! {
            _ = notify_drop.cancelled() => {}
            _ = notify_detach.cancelled() => {
                let mut ep = endpoint_handle.lock().await;
                *ep = None;
            }
        }
    }
}

impl<EH: EndpointHandle> Drop for HotplugEndpointHandleImpl<EH> {
    fn drop(&mut self) {
        // make sure that detach_handler also stops
        self.notify_drop.cancel();
    }
}

impl<EH: EndpointHandle> HotplugEndpointHandle for HotplugEndpointHandleImpl<EH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<HotplugTrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        assert!(
            matches!(
                self.submission_state,
                HotplugSubmissionState::NoTrbSubmitted
            ),
            "submit_trb called twice without calling next_completion"
        );

        // independently of whether we forward the TRB to the endpoint handle, we need
        // to track that we submitted the TRB. We have to send transaction error event
        // with TRB address even if device is gone.
        self.submission_state = HotplugSubmissionState::TrbSubmitted {
            address: trb.address,
        };
        self.restart_position = None;
        if self.transfer_start.is_none() {
            self.transfer_start = Some(trb.clone());
        }

        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            if let Some(device) = guard.as_mut() {
                device.submit_trb(trb)?;
            } else {
                trace!("TRB submission for detached device");
            }
        } else {
            trace!("try_lock failed during submit_trb: detach_handler currently has the lock");
            // After the lock release, the device will be gone, so no need to actually acquire the lock.
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let trb_addr = match self.submission_state {
                HotplugSubmissionState::TrbSubmitted { address, .. } => address,
                HotplugSubmissionState::NoTrbSubmitted => {
                    panic!("next_completion called without prior submit_trb")
                }
            };

            // the event to send in case the real endpoint is/gets detached
            let event = || {
                EventTrb::new_transfer_event_trb(
                    trb_addr,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                )
            };
            let mut transfer_in_progress = false;
            let result = match self.endpoint_handle.lock().await.as_mut() {
                Some(ep) => select! {
                    result = ep.next_completion() => {
                        let result = match result? {
                            TrbProcessingResult::Disconnect => {
                                self.notify_detach.cancel();
                                HotplugTrbProcessingResult::TransactionError(None)
                            },
                            result => HotplugTrbProcessingResult::map_result(result),
                        };
                        transfer_in_progress = ep.transfer_in_progress();
                        result
                    },
                    _ = self.notify_detach.cancelled() => {
                        self.event_sender.send(event())?;
                        HotplugTrbProcessingResult::TransactionError(None)
                    },
                },
                None => {
                    self.event_sender.send(event())?;
                    HotplugTrbProcessingResult::TransactionError(None)
                }
            };

            self.submission_state = HotplugSubmissionState::NoTrbSubmitted;
            if !transfer_in_progress {
                self.transfer_start = None;
            }
            Ok(result)
        })
    }

    fn take_restart_position(&mut self) -> Option<(u64, bool)> {
        self.restart_position.take()
    }
}

impl<EH: EndpointHandle> BaseEndpointHandle for HotplugEndpointHandleImpl<EH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            // The endpoint stops while the xHC works on a TRB. The xHC reports the TRB it stopped
            // on before the completion of the Stop Endpoint command (xHCI 4.6.9). A driver that
            // stops an endpoint with a pending transfer, like the one of a hub that is suspended,
            // waits for this event. The transfer did not start on the device while its TRBs were
            // collected, so it starts again from its first TRB, which the event names.
            let first_trb = self.transfer_start.take();
            if let (HotplugSubmissionState::TrbSubmitted { .. }, Some(first_trb)) =
                (&self.submission_state, &first_trb)
            {
                self.event_sender
                    .send(stopped_event(first_trb, self.endpoint_id, self.slot_id))?;
            }
            // Bit 0 of the fourth dword of a TRB is its cycle bit.
            self.restart_position = first_trb.map(|trb| (trb.address, trb.buffer[12] & 1 != 0));

            if let Ok(mut guard) = self.endpoint_handle.try_lock() {
                if let Some(device) = guard.as_mut() {
                    device.cancel().await?;
                } else {
                    trace!("cancel for detached device");
                }
            } else {
                trace!("try_lock failed during cancel: detach_handler currently has the lock");
                // After the lock release, the device will be gone, so no need to actually acquire the lock.
            }

            self.submission_state = HotplugSubmissionState::NoTrbSubmitted;

            Ok(())
        })
    }
}

impl HotplugEndpointHandleImpl<DummyEndpointHandle> {
    // Necessary because AddressDevice/ConfigureEndpoint might open an endpoint while the device just recently was removed.
    // It makes no sense to fail the command then. So we create a dummy endpoint handle that behaves the same as the
    // endpoint handle of a removed device.
    pub fn dummy(slot_id: u8, endpoint_id: u8, event_sender: EventSender) -> Self {
        Self {
            slot_id,
            endpoint_id,
            event_sender,
            endpoint_handle: Arc::new(Mutex::new(None)),
            submission_state: HotplugSubmissionState::NoTrbSubmitted,
            // just a dummy; nobody will notify, nobody listens
            notify_detach: CancellationToken::new(),
            // just a dummy; the drop implementation will notify, but nobody listens
            notify_drop: CancellationToken::new(),
            transfer_start: None,
            restart_position: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::xhci::{
        interrupter::tests::testutils::MockInterrupter, trb::testutils::RawTrbBuilder,
    };

    #[test]
    fn a_stopped_event_names_the_trb_with_all_of_its_bytes_left() {
        for (trb_type, length) in [
            (trb_types::NORMAL, 100),
            (trb_types::SETUP_STAGE, 8),
            (trb_types::DATA_STAGE, 18),
            (trb_types::ISOCH, 1024),
        ] {
            let trb = RawTrbBuilder::new(0x1000)
                .with_trb_type(trb_type)
                .with_trb_transfer_length(length)
                .build();

            assert_eq!(
                stopped_event(&trb, 5, 3),
                EventTrb::new_transfer_event_trb(
                    0x1000,
                    length,
                    CompletionCode::Stopped,
                    false,
                    5,
                    3
                ),
                "TRB type {trb_type}"
            );
        }
    }

    #[test]
    fn the_length_of_a_trb_that_transfers_no_data_is_not_valid() {
        // An Event Data TRB holds the high bits of its event data where the others hold a length.
        for trb_type in [
            trb_types::STATUS_STAGE,
            trb_types::EVENT_DATA,
            trb_types::NO_OP,
        ] {
            let trb = RawTrbBuilder::new(0x2000)
                .with_trb_type(trb_type)
                .with_byte(8, 0x77)
                .build();

            assert_eq!(
                stopped_event(&trb, 5, 3),
                EventTrb::new_transfer_event_trb(
                    0x2000,
                    0,
                    CompletionCode::StoppedLengthInvalid,
                    false,
                    5,
                    3
                ),
                "TRB type {trb_type}"
            );
        }
    }

    #[tokio::test]
    async fn stopping_an_idle_endpoint_reports_nothing() {
        let (event_sender, interrupter) = MockInterrupter::new();
        let mut handle = HotplugEndpointHandleImpl::dummy(3, 5, event_sender);

        handle.cancel().await.unwrap();

        assert!(interrupter.is_empty());
    }
}
