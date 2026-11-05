use std::collections::VecDeque;

use anyhow::{anyhow, Context};
use tokio::sync::{mpsc, oneshot};
use tokio::{runtime, select};
use tracing::{debug, info, warn};

use crate::device::bus::BusDeviceRef;
use crate::device::interrupt_line::{DummyInterruptLine, InterruptLine};
use crate::device::pci::constants::xhci::runtime::IMOD_DEFAULT;
use crate::device::xhci::controller_reset::ResetSender;
use crate::device::xhci::event_ring::{EnqueueOutcome, EventRing};
use crate::device::xhci::registers::{
    ErdpRegister, ErstbaRegister, ErstszRegister, GenericRwRegister,
};
use crate::device::xhci::trb::EventTrb;
use crate::oneshot_anyhow::SendWithAnyhowError;
use std::sync::Arc;

#[derive(Debug)]
pub struct Interrupter {
    pub registers: InterrupterRegisters,
    /// Transmits events to send to the worker
    msg_sender: mpsc::UnboundedSender<InterrupterMessage>,
}

#[derive(Debug)]
pub struct InterrupterResetSender {
    msg_sender: mpsc::UnboundedSender<InterrupterMessage>,
}

impl ResetSender for InterrupterResetSender {
    fn send_reset(&self, completion_notifier: oneshot::Sender<()>) -> anyhow::Result<()> {
        self.msg_sender
            .send(InterrupterMessage::Reset(completion_notifier))?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct InterrupterRegisters {
    /// IMAN: Interrupt management register
    pub interrupt_management: GenericRwRegister,
    /// IMOD: Interrupt moderation interval
    ///
    /// The minimum interval in 250ns increments between interrupts.
    pub interrupt_moderation_interval: GenericRwRegister,
    /// ERSTSZ: Event ring segment table size
    pub erst_size: ErstszRegister,
    /// ERSTBA: Event ring segment table base address
    pub erst_base_address: ErstbaRegister,
    /// ERDP: Event ring dequeue pointer
    pub eventring_dequeue_pointer: ErdpRegister,
}

impl Default for InterrupterRegisters {
    fn default() -> Self {
        Self {
            interrupt_management: Default::default(),
            interrupt_moderation_interval: GenericRwRegister::new(IMOD_DEFAULT),
            erst_size: Default::default(),
            erst_base_address: Default::default(),
            eventring_dequeue_pointer: Default::default(),
        }
    }
}

#[derive(Debug)]
struct EventWorker {
    registers: InterrupterRegisters,
    msg_recv: mpsc::UnboundedReceiver<InterrupterMessage>,
    interrupt_line: Arc<dyn InterruptLine>,
    event_ring: EventRing,
    /// Events that did not fit into the event ring yet, oldest first.
    pending_events: VecDeque<EventTrb>,
}

#[derive(Debug)]
enum InterrupterMessage {
    SendEvent(EventTrb),
    UpdateInterruptLine(Arc<dyn InterruptLine>),
    Reset(oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub struct EventSender {
    sender: mpsc::UnboundedSender<InterrupterMessage>,
}

impl EventSender {
    pub fn send(&self, event: EventTrb) -> anyhow::Result<()> {
        let msg = InterrupterMessage::SendEvent(event);
        self.sender.send(msg).context("event channel closed")?;

        Ok(())
    }
}

impl Interrupter {
    pub fn new(
        dma_bus: BusDeviceRef,
        // interrupt_enabled: AtomicBool,
        // interrupt_enable_notifier: Notify,
        // interrupts_pending: AtomicU16,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let (msg_sender, msg_recv) = mpsc::unbounded_channel();
        let registers = InterrupterRegisters::default();

        let interrupter = Self {
            registers: registers.clone(),
            msg_sender,
        };

        let event_ring = EventRing::new(dma_bus);
        let worker = EventWorker {
            registers,
            msg_recv,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
            event_ring,
            pending_events: VecDeque::new(),
        };

        async_runtime.spawn(worker.run());

        interrupter
    }

    pub fn set_interrupt_line(&self, interrupt_line: Arc<dyn InterruptLine>) -> anyhow::Result<()> {
        let msg = InterrupterMessage::UpdateInterruptLine(interrupt_line);
        self.msg_sender.send(msg)?;

        Ok(())
    }

    pub fn create_event_sender(&self) -> EventSender {
        EventSender {
            sender: self.msg_sender.clone(),
        }
    }

    pub fn reset_sender(&self) -> InterrupterResetSender {
        InterrupterResetSender {
            msg_sender: self.msg_sender.clone(),
        }
    }
}

impl EventWorker {
    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => {
                info!("EventWorker stopped {err}");
            }
        }
    }

    async fn run_loop(&mut self) -> anyhow::Result<()> {
        // Each ERSTBA write configures a new event ring. A host controller reset
        // clears that configuration and sends us back to waiting for ERSTBA.
        loop {
            self.wait_for_event_ring_configuration().await?;
            if self
                .event_ring
                .configure(
                    self.registers.erst_base_address.base_address(),
                    self.registers.erst_size.segment_table_size(),
                )
                .is_err()
            {
                warn!(
                    "ignoring the event ring configuration, ERSTSZ has to be written before ERSTBA"
                );
                continue;
            }

            self.run_configured().await?;
        }
    }

    // The first ERSTBA write starts the event ring. Drop events that happen
    // before configuration, but keep processing control messages.
    async fn wait_for_event_ring_configuration(&mut self) -> anyhow::Result<()> {
        loop {
            select! {
                _ = self.registers.erst_base_address.write_notification() => return Ok(()),
                // we cannot use self.next_msg() here because it borrows self mutable, clashing
                // with the borrow of self.registers above
                msg = self.msg_recv.recv() => match msg.ok_or_else(|| anyhow!("event channel closed"))? {
                    InterrupterMessage::SendEvent(_) => {}
                    InterrupterMessage::UpdateInterruptLine(interrupt_line) => self.interrupt_line = interrupt_line,
                    InterrupterMessage::Reset(completion) => {
                        self.reset();
                        completion.send_anyhow(())?;
                    }
                },
            }
        }
    }

    // Process messages while the event ring is configured. A reset clears the
    // current configuration and returns to the outer loop to wait for ERSTBA.
    async fn run_configured(&mut self) -> anyhow::Result<()> {
        loop {
            select! {
                // The driver makes room in a full ring by advancing the dequeue pointer.
                _ = self.registers.eventring_dequeue_pointer.write_notification(),
                    if !self.pending_events.is_empty() => self.flush_pending_events(),
                // we cannot use self.next_msg() here because it borrows self mutable, clashing
                // with the borrow of self.registers above
                msg = self.msg_recv.recv() => match msg.ok_or_else(|| anyhow!("event channel closed"))? {
                    InterrupterMessage::SendEvent(event_trb) => {
                        self.pending_events.push_back(event_trb);
                        self.flush_pending_events();
                    }
                    InterrupterMessage::UpdateInterruptLine(interrupt_line) => {
                        self.interrupt_line = interrupt_line;
                        debug!("Updated interrupt line");
                    }
                    InterrupterMessage::Reset(completion) => {
                        self.reset();
                        completion.send_anyhow(())?;
                        break;
                    }
                },
            }
        }

        Ok(())
    }

    /// Put as many pending events as possible onto the event ring.
    fn flush_pending_events(&mut self) {
        while let Some(event_trb) = self.pending_events.front() {
            let outcome = self.event_ring.enqueue(
                event_trb,
                self.registers.erst_size.segment_table_size(),
                self.registers.erst_base_address.base_address(),
                self.registers.eventring_dequeue_pointer.dequeue_pointer(),
            );

            match outcome {
                EnqueueOutcome::Enqueued => {
                    self.interrupt_line.interrupt();
                    if let Some(event_trb) = self.pending_events.pop_front() {
                        debug!("Sent event: {event_trb:?}");
                    }
                }
                EnqueueOutcome::FullErrorPosted => {
                    // The driver learns about the full ring from the error event.
                    self.interrupt_line.interrupt();
                    warn!(
                        "The event ring is full, {} events wait for the driver",
                        self.pending_events.len()
                    );
                    return;
                }
                EnqueueOutcome::Full => return,
            }
        }
    }

    fn reset(&mut self) {
        self.registers.interrupt_management.write(0);
        self.registers
            .interrupt_moderation_interval
            .write(IMOD_DEFAULT);
        self.registers.erst_base_address.reset();
        self.registers.erst_size.write(0);
        self.registers.eventring_dequeue_pointer.write(0);
        self.event_ring.reset();
        self.pending_events.clear();
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::device::xhci::trb::CompletionCode;

    pub mod testutils {
        use std::time::Duration;

        use tokio::time::timeout;

        use super::*;

        const ASYNC_TIMEOUT_SECS: u64 = 30;

        pub struct MockInterrupter {
            timeout_sec: Duration,
            msg_recv: mpsc::UnboundedReceiver<InterrupterMessage>,
        }

        impl MockInterrupter {
            pub fn new() -> (EventSender, Self) {
                let (sender, recv) = mpsc::unbounded_channel();
                let event_sender = EventSender { sender };
                let dummy = Self {
                    timeout_sec: Duration::from_secs(ASYNC_TIMEOUT_SECS),
                    msg_recv: recv,
                };

                (event_sender, dummy)
            }

            pub fn is_empty(&self) -> bool {
                self.msg_recv.is_empty()
            }

            /// receiving anything else than a EventTrb will return None
            pub async fn await_event(&mut self) -> Option<EventTrb> {
                match timeout(self.timeout_sec, self.msg_recv.recv()).await {
                    Ok(Some(InterrupterMessage::SendEvent(event_trb))) => Some(event_trb),
                    _ => None,
                }
            }
        }

        mod tests {
            use super::*;

            #[tokio::test]
            async fn send_event_through_dummy_interrupter() {
                let (event_sender, mut interrupter) = MockInterrupter::new();

                let event = EventTrb::new_port_status_change_event_trb(1);
                matches!(event_sender.send(event), Ok(()));

                assert!(!interrupter.is_empty());
                assert_eq!(
                    interrupter.await_event().await,
                    Some(EventTrb::new_port_status_change_event_trb(1))
                );
            }
        }
    }

    mod ring_full {
        use std::{sync::Arc, time::Duration};

        use tokio::time::sleep;

        use crate::{device::bus::testutils::TestBusDevice, dynamic_bus::DynamicBus};

        use super::*;

        const SEGMENT_TABLE: u64 = 0x0;
        const SEGMENT_BASE: u64 = 0x100;
        const SEGMENT_TRBS: u64 = 16;

        /// Count the TRBs of the single event ring segment that the driver can see.
        fn trbs_with_cycle_bit(ram: &TestBusDevice, cycle: bool) -> usize {
            (0..SEGMENT_TRBS)
                .filter(|slot| {
                    let mut trb = [0u8; 16];
                    ram.read_bulk(SEGMENT_BASE + slot * 16, &mut trb);
                    (trb[12] & 1 != 0) == cycle
                })
                .count()
        }

        async fn wait_for(mut condition: impl FnMut() -> bool) {
            for _ in 0..200 {
                if condition() {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
            panic!("timed out waiting for the interrupter");
        }

        #[tokio::test]
        async fn events_wait_in_a_full_ring_until_the_driver_advances_the_dequeue_pointer() {
            let ram = Arc::new(TestBusDevice::new(&[0u8; 0x400]));
            let dma_bus = Arc::new(DynamicBus::new());
            dma_bus.add(0, ram.clone()).unwrap();
            let interrupter = Interrupter::new(dma_bus, &tokio::runtime::Handle::current());
            let sender = interrupter.create_event_sender();

            // Segment table with a single entry for the ring segment.
            ram.write_bulk(SEGMENT_TABLE, &SEGMENT_BASE.to_le_bytes());
            ram.write_bulk(SEGMENT_TABLE + 8, &(SEGMENT_TRBS as u32).to_le_bytes());

            // Let the worker start, so that it waits for the ERSTBA write.
            sleep(Duration::from_millis(20)).await;

            let registers = &interrupter.registers;
            registers.erst_size.write(1);
            registers.eventring_dequeue_pointer.write(SEGMENT_BASE);
            registers.erst_base_address.write(SEGMENT_TABLE);
            // Let the worker configure the ring before the first event arrives.
            sleep(Duration::from_millis(50)).await;

            // Four more events than the ring has room for.
            for port_id in 0..SEGMENT_TRBS + 4 {
                sender
                    .send(EventTrb::new_port_status_change_event_trb(
                        port_id as u8 + 1,
                    ))
                    .unwrap();
            }

            // The ring holds the events and the error that tells the driver it is full.
            wait_for(|| trbs_with_cycle_bit(&ram, true) == SEGMENT_TRBS as usize).await;
            let mut last_slot = [0u8; 16];
            ram.read_bulk(SEGMENT_BASE + (SEGMENT_TRBS - 1) * 16, &mut last_slot);
            assert_eq!(last_slot[11], u8::from(CompletionCode::EventRingFullError));

            // The driver consumed five TRBs. The waiting events use the freed room except for the
            // slot that reports the ring as full again.
            registers
                .eventring_dequeue_pointer
                .write(SEGMENT_BASE + 5 * 16);
            wait_for(|| trbs_with_cycle_bit(&ram, false) == 5).await;
            let mut first_slot = [0u8; 16];
            ram.read_bulk(SEGMENT_BASE, &mut first_slot);
            assert_eq!(
                first_slot[3], SEGMENT_TRBS as u8,
                "the oldest waiting event, the one for port 16, comes first on the ring"
            );
            let mut error_slot = [0u8; 16];
            ram.read_bulk(SEGMENT_BASE + 4 * 16, &mut error_slot);
            assert_eq!(error_slot[11], u8::from(CompletionCode::EventRingFullError));
        }
    }
}
