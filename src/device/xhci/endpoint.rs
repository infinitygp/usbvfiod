use anyhow::anyhow;
use tokio::{
    runtime, select,
    sync::{mpsc, oneshot},
};
use tracing::{trace, warn};

use crate::{
    device::{
        bus::BusDeviceRef,
        pci::constants::xhci::device_slots::endpoint_state,
        xhci::{
            hotplug_endpoint_handle::HotplugEndpointHandle,
            hotplug_endpoint_handle::HotplugTrbProcessingResult, linked_ring::LinkedRing,
            slot_manager::EndpointContext, trb::CompletionCode,
        },
    },
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct EndpointWorker<EH: HotplugEndpointHandle> {
    state: WorkerState,
    context: EndpointContext,
    transfer_ring: LinkedRing,
    recv: mpsc::UnboundedReceiver<EndpointMessage>,
    real_endpoint: EH,
}

#[derive(Debug)]
enum WorkerState {
    WaitForDoorbell,
    LookForTrb,
    WaitForTrbCompletion,
    Halted,
    Error,
    StoppedWithContinuableTrb,
    // contains the new pointer + cycle state
    SettingTrDequeuePointer(u64, bool, oneshot::Sender<CompletionCode>),
    Stopped,
    Terminating(oneshot::Sender<()>),
}

#[derive(Debug)]
pub enum EndpointMessage {
    Doorbell,
    Stop(oneshot::Sender<CompletionCode>),
    Reset(oneshot::Sender<CompletionCode>),
    // contains the new pointer
    SetTrDequeuePointer(u64, bool, oneshot::Sender<CompletionCode>),
    Terminate(oneshot::Sender<()>),
}

impl EndpointMessage {
    fn try_get_completion_code_sender(self) -> Option<oneshot::Sender<CompletionCode>> {
        match self {
            Self::Doorbell => None,
            Self::SetTrDequeuePointer(_, _, sender) => Some(sender),
            Self::Reset(sender) => Some(sender),
            Self::Stop(sender) => Some(sender),
            Self::Terminate(_) => None,
        }
    }
}

impl<EH: HotplugEndpointHandle> EndpointWorker<EH> {
    pub fn launch(
        async_runtime: &runtime::Handle,
        dma_bus: BusDeviceRef,
        trb_consumer: EH,
        context: EndpointContext,
    ) -> EndpointSender {
        let (sender, recv) = mpsc::unbounded_channel();

        context.set_state(endpoint_state::RUNNING);
        let (dequeue_pointer, cycle_state) = context.get_dequeue_pointer_and_cycle_state();
        let transfer_ring = LinkedRing::new(dma_bus, dequeue_pointer, cycle_state);

        let worker = Self {
            state: WorkerState::WaitForDoorbell,
            context,
            recv,
            real_endpoint: trb_consumer,
            transfer_ring,
        };
        async_runtime.spawn(worker.run());

        EndpointSender { msg_sender: sender }
    }

    async fn run(self) {
        match self.run_loop().await {
            Ok(_) => {
                // endpoint terminated properly
            }
            Err(err) => warn!("endpoint stopped unexpectedly {err}"),
        }
    }

    async fn run_loop(mut self) -> anyhow::Result<()> {
        loop {
            match self.state {
                WorkerState::WaitForDoorbell => match self.next_msg().await? {
                    EndpointMessage::Doorbell => self.state = WorkerState::LookForTrb,
                    EndpointMessage::Stop(sender) => {
                        self.context.set_state(endpoint_state::STOPPED);
                        let (dequeue_pointer, cycle_state) =
                            self.transfer_ring.get_dequeue_pointer();
                        self.context
                            .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                        self.state = WorkerState::Stopped;
                        sender.send_anyhow(CompletionCode::Success)?;
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => self.context_state_error(msg)?,
                },
                WorkerState::LookForTrb => {
                    if let Some(trb) = self.transfer_ring.next_trb() {
                        self.real_endpoint.submit_trb(trb)?;
                        self.state = WorkerState::WaitForTrbCompletion;
                    } else {
                        self.state = WorkerState::WaitForDoorbell;
                    }
                }
                WorkerState::WaitForTrbCompletion => select! {
                    result = self.real_endpoint.next_completion() => match result? {
                        HotplugTrbProcessingResult::Ok => {
                            self.transfer_ring.advance();
                            self.state = WorkerState::LookForTrb;
                        }
                        HotplugTrbProcessingResult::Stall(addr) => {
                            if let Some((addr, cs)) = addr {
                                self.transfer_ring.set_dequeue_pointer(addr, cs);
                            }
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        }
                        HotplugTrbProcessingResult::TransactionError(addr) => {
                            if let Some((addr, cs)) = addr {
                                self.transfer_ring.set_dequeue_pointer(addr, cs);
                            }
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        }
                        HotplugTrbProcessingResult::TrbError => {
                            self.context.set_state(endpoint_state::ERROR);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Error;
                        }
                    },
                    // cannot use self.next_msg() because the &mut it takes clashes with the self.real_endpoint above
                    msg = self.recv.recv() => match msg.ok_or_else(|| anyhow!(""))? {
                        EndpointMessage::Stop(completion) => {
                            self.context.set_state(endpoint_state::STOPPED);
                            self.real_endpoint.cancel().await?;
                            // The endpoint handle works on a whole TD at a time, so the TD starts
                            // over when the endpoint continues.
                            if let Some((address, cycle_state)) = self.real_endpoint.take_restart_position() {
                                self.transfer_ring.set_dequeue_pointer(address, cycle_state);
                            }
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::StoppedWithContinuableTrb;
                            completion.send_anyhow(CompletionCode::Success)?;
                        }
                        EndpointMessage::Doorbell => {}
                        // The endpoint is disabled while a transfer is pending, like when the
                        // driver resets the device or the controller. The device forgets the
                        // transfer, and the driver gets no event for it.
                        EndpointMessage::Terminate(sender) => {
                            self.real_endpoint.abandon().await?;
                            self.state = WorkerState::Terminating(sender);
                        }
                        msg => self.context_state_error(msg)?,
                    }
                },
                WorkerState::Halted => match self.next_msg().await? {
                    EndpointMessage::Reset(completion) => {
                        self.context.set_state(endpoint_state::STOPPED);
                        let (dequeue_pointer, cycle_state) =
                            self.transfer_ring.get_dequeue_pointer();
                        self.context
                            .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                        self.state = WorkerState::Stopped;
                        completion.send_anyhow(CompletionCode::Success)?;
                    }
                    EndpointMessage::Stop(completion) => {
                        // XHCI spec 4.8.3:
                        // A Stop Endpoint Command received while an endpoint is in the Halted
                        // state shall have no effect and shall generate a Command Completion Event with
                        // the Completion Code set to Context State Error.
                        completion.send_anyhow(CompletionCode::ContextStateError)?;
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => self.context_state_error(msg)?,
                },
                WorkerState::Error => match self.next_msg().await? {
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion);
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => self.context_state_error(msg)?,
                },
                // TODO: The WorkerState::StoppedWithContinuableTrb state is
                // currently identical to Stopped and not fully implemented.
                WorkerState::StoppedWithContinuableTrb => match self.next_msg().await? {
                    EndpointMessage::Doorbell => {
                        self.context.set_state(endpoint_state::RUNNING);
                        self.state = WorkerState::LookForTrb;
                    }
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion);
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => self.context_state_error(msg)?,
                },
                WorkerState::Stopped => match self.next_msg().await? {
                    EndpointMessage::Doorbell => {
                        self.context.set_state(endpoint_state::RUNNING);
                        self.state = WorkerState::LookForTrb;
                    }
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion);
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => self.context_state_error(msg)?,
                },
                WorkerState::SettingTrDequeuePointer(ptr, cs, completion) => {
                    // we might be transitioning from Error/Halted;
                    // we could have been in Stopped* state before, then
                    // writing state is unnecessary but also not wrong.
                    self.context.set_state(endpoint_state::STOPPED);
                    self.transfer_ring.set_dequeue_pointer(ptr, cs);
                    self.context.set_dequeue_pointer_and_cycle_state(ptr, cs);
                    self.state = WorkerState::Stopped;
                    completion.send_anyhow(CompletionCode::Success)?;
                }
                WorkerState::Terminating(sender) => {
                    self.context.set_state(endpoint_state::DISABLED);
                    sender.send_anyhow(())?;
                    break;
                }
            }
        }

        Ok(())
    }

    async fn next_msg(&mut self) -> anyhow::Result<EndpointMessage> {
        let msg = self
            .recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("endpoint channel closed"))?;

        trace!("endpoint received: {msg:?}");

        Ok(msg)
    }

    fn context_state_error(&self, msg: EndpointMessage) -> anyhow::Result<()> {
        warn!("invalid endpoint action: {msg:?} in state {:?}", self.state);
        if let Some(sender) = msg.try_get_completion_code_sender() {
            sender.send_anyhow(CompletionCode::ContextStateError)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct EndpointSender {
    msg_sender: mpsc::UnboundedSender<EndpointMessage>,
}

impl EndpointSender {
    /// The sender of an endpoint whose worker is gone.
    #[cfg(test)]
    pub fn closed() -> Self {
        Self {
            msg_sender: mpsc::unbounded_channel().0,
        }
    }

    /// Tell the worker something. A worker that is gone because its endpoint failed does not fail
    /// the slot that the endpoint belongs to: a command for the endpoint is answered with an
    /// error.
    fn send(&self, message: EndpointMessage) {
        if let Err(err) = self.msg_sender.send(message) {
            warn!("the worker of the endpoint is gone, dropping {:?}", err.0);
            if let Some(completion) = err.0.try_get_completion_code_sender() {
                let _ = completion.send(CompletionCode::EndpointNotEnabledError);
            }
        }
    }

    pub fn doorbell(&self) -> anyhow::Result<()> {
        self.send(EndpointMessage::Doorbell);

        Ok(())
    }

    pub fn stop(&self, completion: oneshot::Sender<CompletionCode>) -> anyhow::Result<()> {
        self.send(EndpointMessage::Stop(completion));

        Ok(())
    }

    pub fn reset(&self, completion: oneshot::Sender<CompletionCode>) -> anyhow::Result<()> {
        self.send(EndpointMessage::Reset(completion));

        Ok(())
    }

    pub fn set_tr_dequeue_pointer(
        &self,
        dequeue_pointer: u64,
        cycle_state: bool,
        completion: oneshot::Sender<CompletionCode>,
    ) -> anyhow::Result<()> {
        self.send(EndpointMessage::SetTrDequeuePointer(
            dequeue_pointer,
            cycle_state,
            completion,
        ));

        Ok(())
    }

    pub async fn terminate(&self) -> anyhow::Result<()> {
        let (send, recv) = oneshot::channel();
        self.msg_sender.send(EndpointMessage::Terminate(send))?;
        recv.await?;

        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use std::{
        future::{pending, Future},
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use tokio::time::timeout;

    use super::*;
    use crate::device::{
        bus::{BusDevice, Request, RequestSize},
        pcap::EndpointPcapMeta,
        xhci::{
            endpoint_handle::{ControlEndpointHandle, TdBasedInEndpointHandle},
            hotplug_endpoint_handle::BaseEndpointHandle,
            real_endpoint_handle::{
                ControlRequestProcessingResult, InTrbProcessingResult, RealControlEndpointHandle,
                RealInEndpointHandle,
            },
            trb::{testutils::RawTrbBuilder, RawTrb},
            usbrequest::UsbRequest,
        },
    };

    pub mod testutils {
        use tokio::runtime::Handle;
        use tokio_util::sync::CancellationToken;

        use super::*;
        use crate::{
            device::{
                bus::{testutils::TestBusDevice, BusDevice, BusDeviceRef, Request, RequestSize},
                xhci::{
                    endpoint_handle::EndpointHandle,
                    hotplug_endpoint_handle::HotplugEndpointHandleImpl,
                    interrupter::{tests::testutils::MockInterrupter, EventSender},
                    slot_manager::EndpointContext,
                    trb::EventTrb,
                },
            },
            dynamic_bus::DynamicBus,
        };

        pub const SLOT_ID: u8 = 1;
        pub const CONTEXT: u64 = 0x100;
        pub const RING: u64 = 0x1000;
        pub const BUFFER: u64 = 0x2000;

        pub const NORMAL: u8 = 1;
        pub const SETUP_STAGE: u8 = 2;
        pub const DATA_STAGE: u8 = 3;
        pub const STATUS_STAGE: u8 = 4;
        pub const EVENT_DATA: u8 = 7;

        /// The TRB at the `index`-th place of the ring, with the cycle bit of the first lap.
        pub fn trb(index: u64, trb_type: u8) -> RawTrbBuilder {
            RawTrbBuilder::new(RING + index * 16)
                .with_trb_type(trb_type)
                .with_cycle()
        }

        /// An endpoint worker on a transfer ring in guest memory.
        pub struct Harness {
            pub sender: EndpointSender,
            pub interrupter: MockInterrupter,
            pub bus: Arc<DynamicBus>,
            pub endpoint_id: u8,
        }

        impl Harness {
            /// The ring starts at `RING` with cycle state 1 and holds `trbs`. `make_handle` gets
            /// the guest memory and the way to send events, and makes the endpoint handle that the
            /// worker drives.
            pub fn launch<EH: EndpointHandle>(
                endpoint_id: u8,
                trbs: &[RawTrb],
                make_handle: impl FnOnce(BusDeviceRef, EventSender) -> EH,
            ) -> Self {
                let bus = Arc::new(DynamicBus::new());
                bus.add(0, Arc::new(TestBusDevice::new(&[0u8; 0x4000])))
                    .unwrap();
                bus.write(Request::new(CONTEXT + 8, RequestSize::Size8), RING | 1);
                for trb in trbs {
                    bus.write_bulk(trb.address, &trb.buffer);
                }

                let (event_sender, interrupter) = MockInterrupter::new();
                let runtime = Handle::current();
                let handle = HotplugEndpointHandleImpl::new(
                    SLOT_ID,
                    endpoint_id,
                    make_handle(bus.clone(), event_sender.clone()),
                    event_sender,
                    CancellationToken::new(),
                    &runtime,
                );
                let sender = EndpointWorker::launch(
                    &runtime,
                    bus.clone(),
                    handle,
                    EndpointContext::new(CONTEXT, bus.clone()),
                );
                Self {
                    sender,
                    interrupter,
                    bus,
                    endpoint_id,
                }
            }

            pub async fn stop(&self) -> CompletionCode {
                let (completion, result) = oneshot::channel();
                self.sender.stop(completion).unwrap();
                timeout(Duration::from_secs(5), result)
                    .await
                    .expect("the Stop Endpoint command completes")
                    .unwrap()
            }

            pub fn dequeue_pointer(&self) -> (u64, bool) {
                EndpointContext::new(CONTEXT, self.bus.clone())
                    .get_dequeue_pointer_and_cycle_state()
            }

            pub fn stopped(&self, pointer: u64, length: u32) -> EventTrb {
                EventTrb::new_transfer_event_trb(
                    pointer,
                    length,
                    CompletionCode::Stopped,
                    false,
                    self.endpoint_id,
                    SLOT_ID,
                )
            }
        }

        /// What the real endpoints of the tests were asked to do.
        #[derive(Debug, Default)]
        pub struct Spy {
            pub in_transfers: Mutex<Vec<usize>>,
            pub control_requests: Mutex<Vec<UsbRequest>>,
            pub cancels: AtomicUsize,
        }

        /// A real endpoint whose transfers never complete, like an interrupt endpoint without data.
        #[derive(Debug)]
        pub struct SlowIn(pub Arc<Spy>);

        impl RealInEndpointHandle for SlowIn {
            type TrbCompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

            fn submit(&mut self, data: usize) -> anyhow::Result<()> {
                self.0.in_transfers.lock().unwrap().push(data);
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(pending())
            }
        }

        impl BaseEndpointHandle for SlowIn {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                self.0.cancels.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        /// A real control endpoint that never answers.
        #[derive(Debug)]
        pub struct SlowControl(pub Arc<Spy>);

        impl RealControlEndpointHandle for SlowControl {
            type TrbCompletionFuture<'a> = Pin<
                Box<
                    dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a,
                >,
            >;

            fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
                self.0.control_requests.lock().unwrap().push(request);
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(pending())
            }
        }

        impl BaseEndpointHandle for SlowControl {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                self.0.cancels.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        /// An interrupt IN endpoint whose transfers never complete.
        pub fn slow_in(trbs: &[RawTrb]) -> (Harness, Arc<Spy>) {
            let spy = Arc::new(Spy::default());
            let endpoint_id = 3;
            let harness = Harness::launch(endpoint_id, trbs, |bus, event_sender| {
                TdBasedInEndpointHandle::new(
                    SLOT_ID,
                    endpoint_id,
                    EndpointPcapMeta::interrupt(1, SLOT_ID, endpoint_id),
                    SlowIn(spy.clone()),
                    bus,
                    event_sender,
                )
            });
            (harness, spy)
        }

        /// A control endpoint that never answers.
        pub fn slow_control(trbs: &[RawTrb]) -> (Harness, Arc<Spy>) {
            let spy = Arc::new(Spy::default());
            let endpoint_id = 1;
            let harness = Harness::launch(endpoint_id, trbs, |bus, event_sender| {
                ControlEndpointHandle::new(
                    SLOT_ID,
                    endpoint_id,
                    EndpointPcapMeta::control(1, SLOT_ID, endpoint_id),
                    SlowControl(spy.clone()),
                    bus,
                    event_sender,
                )
            });
            (harness, spy)
        }
    }

    use testutils::*;

    async fn eventually(mut condition: impl FnMut() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the condition did not become true");
    }

    fn context_state(harness: &Harness) -> u64 {
        harness.bus.read(Request::new(CONTEXT, RequestSize::Size1))
    }

    #[tokio::test]
    async fn terminating_an_endpoint_with_a_pending_transfer_cancels_it_and_completes() {
        let (harness, spy) = slow_in(&[trb(0, NORMAL)
            .with_trb_transfer_length(8)
            .with_data_pointer(BUFFER)
            .build()]);
        harness.sender.doorbell().unwrap();
        eventually(|| *spy.in_transfers.lock().unwrap() == [8]).await;

        timeout(Duration::from_secs(5), harness.sender.terminate())
            .await
            .expect("the endpoint answers")
            .expect("the endpoint terminates");

        assert_eq!(spy.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(context_state(&harness), u64::from(endpoint_state::DISABLED));
    }

    #[tokio::test]
    async fn terminating_an_endpoint_in_the_error_state_completes() {
        // A Setup Stage TRB on an interrupt endpoint is a TRB Error.
        let (harness, _spy) = slow_in(&[trb(0, SETUP_STAGE).build()]);
        harness.sender.doorbell().unwrap();
        eventually(|| context_state(&harness) == u64::from(endpoint_state::ERROR)).await;

        timeout(Duration::from_secs(5), harness.sender.terminate())
            .await
            .expect("the endpoint answers")
            .expect("the endpoint terminates");

        assert_eq!(context_state(&harness), u64::from(endpoint_state::DISABLED));
    }

    #[tokio::test]
    async fn an_endpoint_whose_worker_is_gone_answers_commands_with_an_error() {
        let sender = EndpointSender::closed();

        sender.doorbell().unwrap();
        let (completion, result) = oneshot::channel();
        sender.stop(completion).unwrap();
        assert_eq!(
            result.await.unwrap(),
            CompletionCode::EndpointNotEnabledError
        );
        let (completion, result) = oneshot::channel();
        sender.reset(completion).unwrap();
        assert_eq!(
            result.await.unwrap(),
            CompletionCode::EndpointNotEnabledError
        );
        let (completion, result) = oneshot::channel();
        sender
            .set_tr_dequeue_pointer(0x1000, true, completion)
            .unwrap();
        assert_eq!(
            result.await.unwrap(),
            CompletionCode::EndpointNotEnabledError
        );
        sender.terminate().await.unwrap_err();
    }

    #[tokio::test]
    async fn stopping_in_the_middle_of_a_td_rewinds_to_its_first_trb() {
        // Two chained Normal TRBs of 100 and 60 bytes, and an Event Data TRB that ends the TD.
        let (mut harness, spy) = slow_in(&[
            trb(0, NORMAL)
                .with_trb_transfer_length(100)
                .with_data_pointer(BUFFER)
                .with_chain()
                .build(),
            trb(1, NORMAL)
                .with_trb_transfer_length(60)
                .with_data_pointer(BUFFER + 100)
                .with_chain()
                .build(),
            trb(2, EVENT_DATA)
                .with_data_pointer(0xda7a)
                .with_interrupt_on_completion()
                .build(),
        ]);
        harness.sender.doorbell().unwrap();
        eventually(|| *spy.in_transfers.lock().unwrap() == [160]).await;

        assert_eq!(harness.stop().await, CompletionCode::Success);

        // Nothing was transferred, so the Stopped event is for the first TRB of the TD with all of
        // its bytes left, and it is the only event.
        assert_eq!(
            harness.interrupter.await_event().await,
            Some(harness.stopped(RING, 100))
        );
        assert!(harness.interrupter.is_empty());
        assert_eq!(harness.dequeue_pointer(), (RING, true));
        assert_eq!(spy.cancels.load(Ordering::SeqCst), 1);

        // The doorbell starts the TD again from its first TRB.
        harness.sender.doorbell().unwrap();
        eventually(|| *spy.in_transfers.lock().unwrap() == [160, 160]).await;
    }

    #[tokio::test]
    async fn stopping_a_single_trb_td_reports_that_trb() {
        let (mut harness, spy) = slow_in(&[trb(0, NORMAL)
            .with_trb_transfer_length(2)
            .with_data_pointer(BUFFER)
            .with_interrupt_on_completion()
            .build()]);
        harness.sender.doorbell().unwrap();
        eventually(|| *spy.in_transfers.lock().unwrap() == [2]).await;

        assert_eq!(harness.stop().await, CompletionCode::Success);

        assert_eq!(
            harness.interrupter.await_event().await,
            Some(harness.stopped(RING, 2))
        );
        assert!(harness.interrupter.is_empty());
        assert_eq!(harness.dequeue_pointer(), (RING, true));
    }

    #[tokio::test]
    async fn stopping_a_control_transfer_rewinds_to_its_setup_stage() {
        let (mut harness, spy) = slow_control(&[
            trb(0, SETUP_STAGE)
                .with_immediate_data()
                .with_trb_transfer_length(8)
                .with_setup_type(0x80)
                .with_setup_wlength(18)
                .build(),
            trb(1, DATA_STAGE)
                .with_direction()
                .with_trb_transfer_length(18)
                .with_data_pointer(BUFFER)
                .build(),
            trb(2, STATUS_STAGE).with_interrupt_on_completion().build(),
        ]);
        harness.sender.doorbell().unwrap();
        eventually(|| spy.control_requests.lock().unwrap().len() == 1).await;

        assert_eq!(harness.stop().await, CompletionCode::Success);

        assert_eq!(
            harness.interrupter.await_event().await,
            Some(harness.stopped(RING, 8))
        );
        assert!(harness.interrupter.is_empty());
        assert_eq!(harness.dequeue_pointer(), (RING, true));

        // The whole control transfer is sent to the device again, and only once.
        harness.sender.doorbell().unwrap();
        eventually(|| spy.control_requests.lock().unwrap().len() == 2).await;
        let requests = spy.control_requests.lock().unwrap().clone();
        assert_eq!(requests[0], requests[1]);
        assert_eq!((requests[1].request_type, requests[1].length), (0x80, 18));
        assert!(harness.interrupter.is_empty());
    }
}
