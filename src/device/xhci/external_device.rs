//! A device whose transfers another process serves over a unix stream socket.
//!
//! The controller forwards every transfer of the guest to the process on the other end of the
//! socket, framed like USB/IP (see [`usbvfiod::usbip`] and `docs/users/external-devices.md`): a
//! `CMD_SUBMIT` for each transfer, a `RET_SUBMIT` as its completion, and `CMD_UNLINK` and
//! `RET_UNLINK` to cancel a transfer that waits. The identity of the device is not part of the
//! socket. The descriptors come from the process, in answer to the control requests of the guest,
//! and the speed comes from the hotplug command.
//!
//! One task owns the socket. The endpoint handles talk to it through a channel and wait for the
//! completion of a transfer on a one-shot channel, so that transfers of different endpoints are in
//! flight at the same time and complete in any order. When the process closes the socket or breaks
//! the framing, the task cancels the detach token of the device, and the controller detaches it
//! from the tree like any device that goes away. The same token, cancelled by a detach command,
//! makes the task shut the socket down.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    io,
    net::Shutdown,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::anyhow;
use thiserror::Error;
use tokio::{
    net::UnixStream,
    runtime, select,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};
use usbvfiod::usbip::{
    CmdSubmit, CmdUnlink, DecodeError, Direction, Header, RetSubmit, RetUnlink, HEADER_LEN,
    STATUS_CANCELLED, STATUS_STALL,
};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
        OutTrbProcessingResult, OutTrbProcessingStatus, RealControlEndpointHandle,
        RealInEndpointHandle, RealOutEndpointHandle,
    },
    usbrequest::UsbRequest,
};

/// How long a cancelled transfer may wait for the `RET_UNLINK` of the device. A device that does
/// not answer in this time is broken, and gets detached.
const UNLINK_TIMEOUT: Duration = Duration::from_secs(1);

/// The size of one read from the socket.
const READ_CHUNK: usize = 64 * 1024;

/// The names that the hotplug command gave to the external devices, by the number in their
/// identifier. A name only exists while the device is attached.
static NAMES: Mutex<BTreeMap<u32, String>> = Mutex::new(BTreeMap::new());

/// How the tree listing names the external device with this number in its identifier.
#[must_use]
pub fn describe(number: u32) -> String {
    let names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    names.get(&number).map_or_else(
        || format!("external #{number}"),
        |name| format!("external {name}"),
    )
}

/// Removes the name of a device when the device is gone.
#[derive(Debug)]
struct NameRegistration(u32);

impl NameRegistration {
    fn new(number: u32, name: Option<&str>) -> Option<Self> {
        let name = name?;
        NAMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(number, name.to_string());
        Some(Self(number))
    }
}

impl Drop for NameRegistration {
    fn drop(&mut self) {
        NAMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// What a transfer ended with, as the device reported it.
#[derive(Debug)]
struct Completion {
    status: i32,
    /// The bytes of an IN transfer, empty for an OUT transfer.
    data: Vec<u8>,
    /// The bytes the device returned for an IN transfer, or accepted for an OUT transfer.
    actual_length: u32,
}

/// A request to the task that owns the socket.
#[derive(Debug)]
enum Message {
    Submit(Submit),
    Unlink {
        /// The `CMD_SUBMIT` to cancel.
        seqnum: u32,
        /// Told when the transfer is not in flight any more.
        done: Option<oneshot::Sender<()>>,
    },
}

#[derive(Debug)]
struct Submit {
    seqnum: u32,
    direction: Direction,
    ep: u8,
    setup: [u8; 8],
    /// The `transfer_buffer_length`. For an OUT transfer, `data` has that many bytes.
    length: u32,
    data: Vec<u8>,
    done: oneshot::Sender<Completion>,
}

/// The way to the task that owns the socket, shared by the device and its endpoint handles.
#[derive(Debug, Clone)]
struct Link {
    messages: mpsc::UnboundedSender<Message>,
    /// The next sequence number, for transfers and for unlinks.
    seqnums: Arc<AtomicU32>,
    /// Cancelled when the device is detached. The endpoints cancel it when the device is broken.
    detach: CancellationToken,
    unlink_timeout: Duration,
}

impl Link {
    /// Sequence numbers count from 1 and are unique among the transfers in flight.
    fn next_seqnum(seqnums: &AtomicU32) -> u32 {
        loop {
            let seqnum = seqnums.fetch_add(1, Ordering::Relaxed);
            if seqnum != 0 {
                return seqnum;
            }
        }
    }
}

/// A transfer that was submitted and has not completed.
#[derive(Debug)]
struct Transfer {
    seqnum: u32,
    done: oneshot::Receiver<Completion>,
}

/// What every endpoint handle does with the socket: at most one transfer at a time.
#[derive(Debug)]
struct Endpoint {
    link: Link,
    ep: u8,
    transfer: Option<Transfer>,
}

impl Endpoint {
    const fn new(link: Link, ep: u8) -> Self {
        Self {
            link,
            ep,
            transfer: None,
        }
    }

    fn submit(&mut self, direction: Direction, setup: [u8; 8], length: u32, data: Vec<u8>) {
        if let Some(stale) = self.transfer.take() {
            warn!(
                "ep {}: a new transfer was submitted while transfer {} was in flight",
                self.ep, stale.seqnum
            );
            self.unlink_without_waiting(stale.seqnum);
        }
        let seqnum = Link::next_seqnum(&self.link.seqnums);
        let (done, receiver) = oneshot::channel();
        // If the task is gone, the message and with it the sender are dropped, and the completion
        // reports that the device is gone.
        let _ = self.link.messages.send(Message::Submit(Submit {
            seqnum,
            direction,
            ep: self.ep,
            setup,
            length,
            data,
            done,
        }));
        self.transfer = Some(Transfer {
            seqnum,
            done: receiver,
        });
    }

    /// Wait for the completion of the transfer. The future can be dropped at any time, and the
    /// transfer stays in flight until [`Self::cancel`].
    ///
    /// `None` means that the device is gone.
    async fn completion(&mut self) -> anyhow::Result<Option<Completion>> {
        let transfer = self
            .transfer
            .as_mut()
            .ok_or_else(|| anyhow!("no transfer was submitted"))?;
        let completion = (&mut transfer.done).await.ok();
        self.transfer = None;
        Ok(completion)
    }

    /// Cancel the transfer that waits, and wait until the device says that it is not in flight any
    /// more. A device that does not say so in time is detached.
    async fn cancel(&mut self) -> anyhow::Result<()> {
        let Some(mut transfer) = self.transfer.take() else {
            return Ok(());
        };
        match transfer.done.try_recv() {
            // The transfer completed before the cancel, which throws the result away.
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => return Ok(()),
            Err(oneshot::error::TryRecvError::Empty) => {}
        }

        let (done, unlinked) = oneshot::channel();
        let message = Message::Unlink {
            seqnum: transfer.seqnum,
            done: Some(done),
        };
        if self.link.messages.send(message).is_err() {
            return Ok(());
        }
        if timeout(self.link.unlink_timeout, unlinked).await.is_err() {
            warn!(
                "ep {}: the device did not answer the unlink of transfer {} in {:?}, detaching it",
                self.ep, transfer.seqnum, self.link.unlink_timeout
            );
            self.link.detach.cancel();
        }
        Ok(())
    }

    fn unlink_without_waiting(&self, seqnum: u32) {
        let _ = self
            .link
            .messages
            .send(Message::Unlink { seqnum, done: None });
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // A transfer that nobody waits for must not keep a report of the device.
        if let Some(transfer) = self.transfer.take() {
            self.unlink_without_waiting(transfer.seqnum);
        }
    }
}

/// How the guest sees the end of a transfer.
enum Outcome {
    Success,
    Stall,
    TransactionError,
}

fn outcome(status: i32) -> Outcome {
    match status {
        0 => Outcome::Success,
        STATUS_STALL => Outcome::Stall,
        other => {
            debug!("a transfer ended with the status {other}");
            Outcome::TransactionError
        }
    }
}

/// A device that another process serves over a unix stream socket.
#[derive(Debug)]
pub struct ExternalDevice {
    speed: Speed,
    link: Link,
}

impl ExternalDevice {
    /// Serve the device on the socket. The task ends, and the socket is shut down, when `detach`
    /// is cancelled. It cancels `detach` itself when the other end goes away.
    ///
    /// `number` is the number in the identifier of the device, which the name is kept under.
    pub fn start(
        socket: UnixStream,
        speed: Speed,
        number: u32,
        name: Option<String>,
        detach: CancellationToken,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let (messages, receiver) = mpsc::unbounded_channel();
        let seqnums = Arc::new(AtomicU32::new(1));
        let label = name.clone().unwrap_or_else(|| format!("#{number}"));
        let registration = NameRegistration::new(number, name.as_deref());

        let session = Session {
            socket,
            messages: receiver,
            seqnums: seqnums.clone(),
            detach: detach.clone(),
            label,
            pending: HashMap::new(),
            unlinks: HashMap::new(),
            input: Vec::new(),
            output: Vec::new(),
        };
        async_runtime.spawn(async move {
            session.serve().await;
            drop(registration);
        });

        Self {
            speed,
            link: Link {
                messages,
                seqnums,
                detach,
                unlink_timeout: UNLINK_TIMEOUT,
            },
        }
    }

    /// Wait for the answer to an unlink for a shorter time.
    #[cfg(test)]
    #[must_use]
    const fn with_unlink_timeout(mut self, unlink_timeout: Duration) -> Self {
        self.link.unlink_timeout = unlink_timeout;
        self
    }
}

impl RealDevice for ExternalDevice {
    type RCEH = ExternalControlEndpoint;
    type RBIEH = ExternalInEndpoint;
    type RBOEH = ExternalOutEndpoint;
    type RIIEH = ExternalInEndpoint;
    type RIOEH = ExternalOutEndpoint;

    fn speed(&self) -> Option<Speed> {
        Some(self.speed)
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        ExternalControlEndpoint {
            endpoint: Endpoint::new(self.link.clone(), 0),
            direction: Direction::Out,
        }
    }

    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
        ExternalInEndpoint(Endpoint::new(
            self.link.clone(),
            endpoint_number(endpoint_id),
        ))
    }

    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
        ExternalOutEndpoint(Endpoint::new(
            self.link.clone(),
            endpoint_number(endpoint_id),
        ))
    }

    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
        ExternalInEndpoint(Endpoint::new(
            self.link.clone(),
            endpoint_number(endpoint_id),
        ))
    }

    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
        ExternalOutEndpoint(Endpoint::new(
            self.link.clone(),
            endpoint_number(endpoint_id),
        ))
    }
}

/// The endpoint number of an xHCI device context index: the index is the number times two, plus
/// one for an IN endpoint.
const fn endpoint_number(endpoint_id: u8) -> u8 {
    endpoint_id >> 1
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The control endpoint of an [`ExternalDevice`]. Every request goes to the device, the standard
/// ones as well.
#[derive(Debug)]
pub struct ExternalControlEndpoint {
    endpoint: Endpoint,
    /// The direction of the request in flight.
    direction: Direction,
}

impl BaseEndpointHandle for ExternalControlEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(self.endpoint.cancel())
    }
}

impl RealControlEndpointHandle for ExternalControlEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<ControlRequestProcessingResult>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.direction = if request.request_type & 0x80 == 0 {
            Direction::Out
        } else {
            Direction::In
        };
        let mut data = request.data;
        // wLength is 16 bits wide, and the data stage can be longer than that in a broken TD.
        let wanted = match self.direction {
            Direction::In => request.length,
            Direction::Out => u32::try_from(data.len()).unwrap_or(u32::MAX),
        };
        let length = u16::try_from(wanted).unwrap_or_else(|_| {
            warn!("the data stage of a control request has {wanted} bytes, which is cut to 65535");
            u16::MAX
        });
        match self.direction {
            Direction::In => data.clear(),
            Direction::Out => data.truncate(usize::from(length)),
        }

        let value = request.value.to_le_bytes();
        let index = request.index.to_le_bytes();
        let length_bytes = length.to_le_bytes();
        let setup = [
            request.request_type,
            request.request,
            value[0],
            value[1],
            index[0],
            index[1],
            length_bytes[0],
            length_bytes[1],
        ];
        self.endpoint
            .submit(self.direction, setup, u32::from(length), data);
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let Some(completion) = self.endpoint.completion().await? else {
                return Ok(ControlRequestProcessingResult::Disconnect);
            };
            Ok(match (outcome(completion.status), self.direction) {
                (Outcome::Success, Direction::In) => {
                    ControlRequestProcessingResult::SuccessfulControlIn(completion.data)
                }
                (Outcome::Success, Direction::Out) => {
                    ControlRequestProcessingResult::SuccessfulControlOut
                }
                (Outcome::Stall, _) => ControlRequestProcessingResult::Stall,
                (Outcome::TransactionError, _) => ControlRequestProcessingResult::TransactionError,
            })
        })
    }
}

/// A bulk or interrupt IN endpoint of an [`ExternalDevice`].
#[derive(Debug)]
pub struct ExternalInEndpoint(Endpoint);

impl BaseEndpointHandle for ExternalInEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(self.0.cancel())
    }
}

impl RealInEndpointHandle for ExternalInEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<InTrbProcessingResult>>;

    fn submit(&mut self, data: usize) -> anyhow::Result<()> {
        let length = u32::try_from(data)
            .map_err(|_| anyhow!("an IN transfer of {data} bytes is too long"))?;
        self.0.submit(Direction::In, [0; 8], length, Vec::new());
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let Some(completion) = self.0.completion().await? else {
                return Ok(InTrbProcessingResult {
                    status: InTrbProcessingStatus::Disconnect,
                    data: Vec::new(),
                });
            };
            let status = match outcome(completion.status) {
                Outcome::Success => InTrbProcessingStatus::Success,
                Outcome::Stall => InTrbProcessingStatus::Stall,
                Outcome::TransactionError => InTrbProcessingStatus::TransactionError,
            };
            Ok(InTrbProcessingResult {
                status,
                data: completion.data,
            })
        })
    }
}

/// A bulk or interrupt OUT endpoint of an [`ExternalDevice`].
#[derive(Debug)]
pub struct ExternalOutEndpoint(Endpoint);

impl BaseEndpointHandle for ExternalOutEndpoint {
    type CompletionFuture<'a> = BoxFuture<'a, anyhow::Result<()>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(self.0.cancel())
    }
}

impl RealOutEndpointHandle for ExternalOutEndpoint {
    type TrbCompletionFuture<'a> = BoxFuture<'a, anyhow::Result<OutTrbProcessingResult>>;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let length = u32::try_from(data.len())
            .map_err(|_| anyhow!("an OUT transfer of {} bytes is too long", data.len()))?;
        self.0.submit(Direction::Out, [0; 8], length, data);
        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let Some(completion) = self.0.completion().await? else {
                return Ok(OutTrbProcessingResult {
                    status: OutTrbProcessingStatus::Disconnect,
                    length: 0,
                });
            };
            let status = match outcome(completion.status) {
                Outcome::Success => OutTrbProcessingStatus::Success,
                Outcome::Stall => OutTrbProcessingStatus::Stall,
                Outcome::TransactionError => OutTrbProcessingStatus::TransactionError,
            };
            Ok(OutTrbProcessingResult {
                status,
                length: completion.actual_length as usize,
            })
        })
    }
}

/// A `CMD_SUBMIT` that the device did not answer yet.
#[derive(Debug)]
struct Pending {
    direction: Direction,
    /// The `transfer_buffer_length`, which the answer must not exceed.
    length: u32,
    /// Where the completion goes. It is gone when the transfer was cancelled and the device says
    /// that it completed anyway: its `RET_SUBMIT` still arrives, and is dropped.
    done: Option<oneshot::Sender<Completion>>,
}

/// A `CMD_UNLINK` that the device did not answer yet.
#[derive(Debug)]
struct Unlinking {
    /// The `CMD_SUBMIT` it cancels.
    target: u32,
    done: Option<oneshot::Sender<()>>,
}

/// Why the task stopped serving.
#[derive(Debug)]
enum End {
    /// The device was detached by the controller.
    Detached,
    /// The device closed the socket.
    PeerClosed,
    /// Nobody holds the device any more.
    Abandoned,
}

#[derive(Debug, Error)]
enum Failure {
    #[error("the socket failed: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Header(#[from] DecodeError),
    #[error("{0}")]
    Protocol(String),
}

/// The task that owns the socket.
#[derive(Debug)]
struct Session {
    socket: UnixStream,
    messages: mpsc::UnboundedReceiver<Message>,
    seqnums: Arc<AtomicU32>,
    detach: CancellationToken,
    /// The name of the device in the log.
    label: String,
    pending: HashMap<u32, Pending>,
    unlinks: HashMap<u32, Unlinking>,
    /// Bytes read from the socket that are not a whole message yet.
    input: Vec<u8>,
    /// Bytes waiting to be written to the socket.
    output: Vec<u8>,
}

impl Session {
    async fn serve(mut self) {
        info!("external device {}: serving the socket", self.label);
        match self.run().await {
            Ok(End::Detached) => {
                info!(
                    "external device {}: detached, closing the socket",
                    self.label
                );
            }
            Ok(End::PeerClosed) => {
                info!("external device {}: the socket was closed", self.label);
                self.detach.cancel();
            }
            Ok(End::Abandoned) => {
                debug!("external device {}: nobody holds the device", self.label);
                self.detach.cancel();
            }
            Err(failure) => {
                warn!("external device {}: {failure}, detaching it", self.label);
                self.detach.cancel();
            }
        }

        // Transfers in flight end with the loss of the device, because their senders are dropped.
        // The peer sees the end of the stream even when it holds another descriptor of its end.
        if let Ok(socket) = self.socket.into_std() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    async fn run(&mut self) -> Result<End, Failure> {
        let detach = self.detach.clone();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let has_output = !self.output.is_empty();
            select! {
                () = detach.cancelled() => return Ok(End::Detached),
                message = self.messages.recv() => match message {
                    Some(message) => self.handle(message),
                    None => return Ok(End::Abandoned),
                },
                ready = self.socket.writable(), if has_output => {
                    ready?;
                    self.write_some()?;
                }
                ready = self.socket.readable() => {
                    ready?;
                    if self.read_some(&mut chunk)? {
                        return Ok(End::PeerClosed);
                    }
                    self.process_input()?;
                }
            }
        }
    }

    fn write_some(&mut self) -> Result<(), Failure> {
        match self.socket.try_write(&self.output) {
            Ok(written) => {
                self.output.drain(..written);
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Whether the other end closed the socket.
    fn read_some(&mut self, chunk: &mut [u8]) -> Result<bool, Failure> {
        match self.socket.try_read(chunk) {
            Ok(0) => Ok(true),
            Ok(read) => {
                self.input.extend_from_slice(&chunk[..read]);
                Ok(false)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Submit(submit) => self.submit(submit),
            Message::Unlink { seqnum, done } => self.unlink(seqnum, done),
        }
    }

    fn submit(&mut self, submit: Submit) {
        let header = Header::CmdSubmit(CmdSubmit {
            seqnum: submit.seqnum,
            direction: submit.direction,
            ep: submit.ep,
            transfer_buffer_length: submit.length,
            setup: submit.setup,
        });
        debug!("external device {}: {header}", self.label);
        trace!(
            "external device {}: setup {:02x?}, data {:02x?}",
            self.label,
            submit.setup,
            submit.data
        );
        self.output.extend_from_slice(&header.encode());
        self.output.extend_from_slice(&submit.data);
        self.pending.insert(
            submit.seqnum,
            Pending {
                direction: submit.direction,
                length: submit.length,
                done: Some(submit.done),
            },
        );
    }

    fn unlink(&mut self, target: u32, done: Option<oneshot::Sender<()>>) {
        // A transfer that is not waiting, or was cancelled already, has nothing to unlink.
        if self
            .pending
            .get(&target)
            .is_none_or(|pending| pending.done.is_none())
        {
            if let Some(done) = done {
                let _ = done.send(());
            }
            return;
        }

        let seqnum = Link::next_seqnum(&self.seqnums);
        let header = Header::CmdUnlink(CmdUnlink {
            seqnum,
            unlink_seqnum: target,
        });
        debug!("external device {}: {header}", self.label);
        self.output.extend_from_slice(&header.encode());
        self.unlinks.insert(seqnum, Unlinking { target, done });
    }

    /// Handle all the whole messages in the input.
    fn process_input(&mut self) -> Result<(), Failure> {
        while let Some(bytes) = self.input.first_chunk::<HEADER_LEN>() {
            match Header::decode(bytes)? {
                Header::RetSubmit(ret) => {
                    if !self.ret_submit(ret)? {
                        break;
                    }
                }
                Header::RetUnlink(ret) => {
                    self.input.drain(..HEADER_LEN);
                    self.ret_unlink(ret)?;
                }
                other @ (Header::CmdSubmit(_) | Header::CmdUnlink(_)) => {
                    return Err(Failure::Protocol(format!("the device sent {other}")));
                }
            }
        }
        Ok(())
    }

    /// Complete a transfer. False if the data of an IN transfer has not arrived completely yet.
    fn ret_submit(&mut self, ret: RetSubmit) -> Result<bool, Failure> {
        debug!("external device {}: {}", self.label, Header::RetSubmit(ret));
        let Some(pending) = self.pending.get(&ret.seqnum) else {
            return Err(Failure::Protocol(format!(
                "RET_SUBMIT {} answers no transfer",
                ret.seqnum
            )));
        };
        let (direction, length) = (pending.direction, pending.length);
        let actual_length = u32::try_from(ret.actual_length)
            .ok()
            .filter(|actual| *actual <= length)
            .ok_or_else(|| {
                Failure::Protocol(format!(
                    "RET_SUBMIT {} returns {} bytes of a transfer of {length}",
                    ret.seqnum, ret.actual_length
                ))
            })?;

        let data_length = match direction {
            Direction::In => actual_length as usize,
            Direction::Out => 0,
        };
        if self.input.len() < HEADER_LEN + data_length {
            return Ok(false);
        }
        let data = self.input[HEADER_LEN..HEADER_LEN + data_length].to_vec();
        self.input.drain(..HEADER_LEN + data_length);
        trace!("external device {}: data {data:02x?}", self.label);

        if let Some(done) = self
            .pending
            .remove(&ret.seqnum)
            .and_then(|pending| pending.done)
        {
            // The endpoint may have lost interest in the meantime.
            let _ = done.send(Completion {
                status: ret.status,
                data,
                actual_length,
            });
        }
        Ok(true)
    }

    fn ret_unlink(&mut self, ret: RetUnlink) -> Result<(), Failure> {
        debug!("external device {}: {}", self.label, Header::RetUnlink(ret));
        let Some(unlinking) = self.unlinks.remove(&ret.seqnum) else {
            return Err(Failure::Protocol(format!(
                "RET_UNLINK {} answers no unlink",
                ret.seqnum
            )));
        };

        if ret.status == STATUS_CANCELLED {
            // The device cancelled the transfer, and sends no RET_SUBMIT for it. The transfer may
            // have completed before, if its RET_SUBMIT came first.
            self.pending.remove(&unlinking.target);
        } else if let Some(pending) = self.pending.get_mut(&unlinking.target) {
            // The transfer completed on the device, and its RET_SUBMIT is on its way. Nobody wants
            // the result of a cancelled transfer.
            pending.done = None;
        }
        if let Some(done) = unlinking.done {
            let _ = done.send(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::device::xhci::{
        any_device::{AnyDevice, ControllerDevice},
        interrupter::tests::testutils::MockInterrupter,
        port::PortArray,
        real_device::{CompleteRealDevice, CompleteRealDeviceImpl, DeviceId},
    };
    use usbvfiod::{hotplug_protocol::response::Response, usbip::STATUS_TRANSACTION_ERROR};

    /// The device descriptor of a full speed module: USB 1.10, and 8 bytes for endpoint 0.
    const DEVICE_DESCRIPTOR: [u8; 18] = [
        18, 1, 0x10, 0x01, 0, 0, 0, 8, 0x16, 0x25, 0xfe, 0x01, 0x00, 0x01, 1, 2, 3, 1,
    ];

    /// The other end of the socket: the process that serves the device.
    struct Peer(UnixStream);

    impl Peer {
        async fn header(&mut self) -> Header {
            let mut bytes = [0u8; HEADER_LEN];
            timeout(Duration::from_secs(5), self.0.read_exact(&mut bytes))
                .await
                .expect("a header arrives")
                .unwrap();
            Header::decode(&bytes).unwrap()
        }

        /// The next `CMD_SUBMIT`, with the data of an OUT transfer.
        async fn submit(&mut self) -> (CmdSubmit, Vec<u8>) {
            let Header::CmdSubmit(submit) = self.header().await else {
                panic!("a CMD_SUBMIT was expected");
            };
            let data = match submit.direction {
                Direction::In => Vec::new(),
                Direction::Out => {
                    let mut data = vec![0u8; submit.transfer_buffer_length as usize];
                    timeout(Duration::from_secs(5), self.0.read_exact(&mut data))
                        .await
                        .expect("the data arrives")
                        .unwrap();
                    data
                }
            };
            (submit, data)
        }

        async fn unlink(&mut self) -> CmdUnlink {
            let Header::CmdUnlink(unlink) = self.header().await else {
                panic!("a CMD_UNLINK was expected");
            };
            unlink
        }

        async fn send(&mut self, header: Header, data: &[u8]) {
            self.0.write_all(&header.encode()).await.unwrap();
            self.0.write_all(data).await.unwrap();
        }

        async fn complete(&mut self, seqnum: u32, status: i32, data: &[u8]) {
            let actual_length = i32::try_from(data.len()).unwrap();
            self.send(
                Header::RetSubmit(RetSubmit {
                    seqnum,
                    status,
                    actual_length,
                }),
                data,
            )
            .await;
        }

        /// Complete an OUT transfer that accepted this many bytes.
        async fn accept(&mut self, seqnum: u32, accepted: i32) {
            self.send(
                Header::RetSubmit(RetSubmit {
                    seqnum,
                    status: 0,
                    actual_length: accepted,
                }),
                &[],
            )
            .await;
        }

        async fn answer_unlink(&mut self, seqnum: u32, status: i32) {
            self.send(Header::RetUnlink(RetUnlink { seqnum, status }), &[])
                .await;
        }

        /// Whether the controller closed its end, which is what a read of the end reports.
        async fn closed(&mut self) -> bool {
            let mut byte = [0u8; 1];
            matches!(
                timeout(Duration::from_secs(5), self.0.read(&mut byte)).await,
                Ok(Ok(0))
            )
        }
    }

    fn device_at(speed: Speed) -> (ExternalDevice, Peer, CancellationToken) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let detach = CancellationToken::new();
        let device = ExternalDevice::start(
            ours,
            speed,
            1000,
            None,
            detach.clone(),
            &runtime::Handle::current(),
        );
        (device, Peer(theirs), detach)
    }

    fn device() -> (ExternalDevice, Peer, CancellationToken) {
        device_at(Speed::Full)
    }

    fn get_descriptor(length: u32) -> UsbRequest {
        UsbRequest {
            request_type: 0x80,
            request: 6,
            value: 0x0100,
            index: 0,
            length,
            ..Default::default()
        }
    }

    async fn eventually(mut condition: impl FnMut() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the condition did not become true");
    }

    #[tokio::test]
    async fn the_device_descriptor_arrives_as_the_peer_wrote_it() {
        let (device, mut peer, _detach) = device();
        let mut control = device.control_endpoint_handle();

        control.submit_control_request(get_descriptor(64)).unwrap();
        let (submit, data) = peer.submit().await;
        assert_eq!(submit.seqnum, 1, "sequence numbers count from 1");
        assert_eq!((submit.direction, submit.ep), (Direction::In, 0));
        assert_eq!(submit.transfer_buffer_length, 64);
        assert_eq!(submit.setup, [0x80, 6, 0x00, 0x01, 0, 0, 64, 0]);
        assert!(data.is_empty());
        peer.complete(submit.seqnum, 0, &DEVICE_DESCRIPTOR).await;

        match control.next_completion().await.unwrap() {
            ControlRequestProcessingResult::SuccessfulControlIn(data) => {
                assert_eq!(data, DEVICE_DESCRIPTOR);
                assert_eq!(data[2..4], [0x10, 0x01], "bcdUSB 1.10");
                assert_eq!(data[7], 8, "bMaxPacketSize0");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(device.speed(), Some(Speed::Full));
    }

    #[tokio::test]
    async fn every_control_request_goes_to_the_device() {
        let (device, mut peer, _detach) = device();
        let mut control = device.control_endpoint_handle();

        // The standard requests that a driver sends, and a class request with data.
        let requests = [
            (0x00, 9, 1, 0, vec![]),
            (0x01, 11, 1, 2, vec![]),
            (0x02, 1, 0, 0x81, vec![]),
            (0x21, 9, 0x0200, 1, vec![1, 2, 3, 4]),
        ];
        for (request_type, request, value, index, data) in requests {
            control
                .submit_control_request(UsbRequest {
                    request_type,
                    request,
                    value,
                    index,
                    data: data.clone(),
                    ..Default::default()
                })
                .unwrap();
            let (submit, received) = peer.submit().await;
            assert_eq!((submit.direction, submit.ep), (Direction::Out, 0));
            let length = u16::try_from(data.len()).unwrap();
            let mut setup = vec![request_type, request];
            setup.extend(value.to_le_bytes());
            setup.extend(index.to_le_bytes());
            setup.extend(length.to_le_bytes());
            assert_eq!(submit.setup.to_vec(), setup);
            assert_eq!(submit.transfer_buffer_length, u32::from(length));
            assert_eq!(received, data, "the data stage follows the header");
            peer.accept(submit.seqnum, i32::from(length)).await;
            assert!(matches!(
                control.next_completion().await.unwrap(),
                ControlRequestProcessingResult::SuccessfulControlOut
            ));
        }
    }

    #[tokio::test]
    async fn statuses_map_to_the_results_of_the_endpoints() {
        let (device, mut peer, detach) = device();
        let mut control = device.control_endpoint_handle();
        let mut interrupt_in = device.interrupt_in_endpoint_handle(3);
        let mut bulk_out = device.bulk_out_endpoint_handle(4);

        control.submit_control_request(get_descriptor(18)).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, STATUS_STALL, &[]).await;
        assert!(matches!(
            control.next_completion().await.unwrap(),
            ControlRequestProcessingResult::Stall
        ));

        control.submit_control_request(get_descriptor(18)).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, STATUS_TRANSACTION_ERROR, &[])
            .await;
        assert!(matches!(
            control.next_completion().await.unwrap(),
            ControlRequestProcessingResult::TransactionError
        ));

        // Any other negative status is a transaction error, too.
        interrupt_in.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, -110, &[]).await;
        assert!(matches!(
            interrupt_in.next_completion().await.unwrap().status,
            InTrbProcessingStatus::TransactionError
        ));

        interrupt_in.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, STATUS_STALL, &[]).await;
        assert!(matches!(
            interrupt_in.next_completion().await.unwrap().status,
            InTrbProcessingStatus::Stall
        ));

        bulk_out.submit(vec![1, 2, 3]).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, STATUS_STALL, &[]).await;
        assert!(matches!(
            bulk_out.next_completion().await.unwrap().status,
            OutTrbProcessingStatus::Stall
        ));

        assert!(!detach.is_cancelled(), "a stall is not a broken device");
    }

    #[tokio::test]
    async fn an_interrupt_transfer_completes_late_while_the_control_endpoint_works() {
        let (device, mut peer, _detach) = device();
        let mut control = device.control_endpoint_handle();
        // Device context index 3 is endpoint 1 IN.
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (waiting, _) = peer.submit().await;
        assert_eq!((waiting.direction, waiting.ep), (Direction::In, 1));
        assert_eq!(waiting.transfer_buffer_length, 8);
        assert_eq!(waiting.setup, [0; 8]);

        // Two control requests complete while the interrupt transfer waits.
        for length in [18, 9] {
            control
                .submit_control_request(get_descriptor(length))
                .unwrap();
            let (submit, _) = peer.submit().await;
            assert_eq!(submit.ep, 0);
            assert_ne!(submit.seqnum, waiting.seqnum);
            peer.complete(submit.seqnum, 0, &DEVICE_DESCRIPTOR[..length as usize])
                .await;
            assert!(matches!(
                control.next_completion().await.unwrap(),
                ControlRequestProcessingResult::SuccessfulControlIn(data)
                    if data == DEVICE_DESCRIPTOR[..length as usize]
            ));
        }

        peer.complete(waiting.seqnum, 0, &[7, 8, 9]).await;
        let completion = interrupt.next_completion().await.unwrap();
        assert!(matches!(completion.status, InTrbProcessingStatus::Success));
        assert_eq!(completion.data, [7, 8, 9], "a short transfer is fine");
    }

    #[tokio::test]
    async fn replies_in_any_order_reach_their_endpoints() {
        let (device, mut peer, _detach) = device();
        let mut control = device.control_endpoint_handle();
        let mut first = device.interrupt_in_endpoint_handle(3);
        let mut second = device.bulk_in_endpoint_handle(5);

        first.submit(4).unwrap();
        let (first_submit, _) = peer.submit().await;
        second.submit(4).unwrap();
        let (second_submit, _) = peer.submit().await;
        control.submit_control_request(get_descriptor(2)).unwrap();
        let (control_submit, _) = peer.submit().await;
        assert_eq!(
            [first_submit.ep, second_submit.ep, control_submit.ep],
            [1, 2, 0]
        );

        peer.complete(control_submit.seqnum, 0, &[0xc0, 0xc1]).await;
        peer.complete(second_submit.seqnum, 0, &[2, 2]).await;
        peer.complete(first_submit.seqnum, 0, &[1]).await;

        assert_eq!(first.next_completion().await.unwrap().data, [1]);
        assert_eq!(second.next_completion().await.unwrap().data, [2, 2]);
        assert!(matches!(
            control.next_completion().await.unwrap(),
            ControlRequestProcessingResult::SuccessfulControlIn(data) if data == [0xc0, 0xc1]
        ));
    }

    #[tokio::test]
    async fn out_data_arrives_with_its_bytes() {
        let (device, mut peer, _detach) = device();
        // Device context index 4 is endpoint 2 OUT, and 2 is endpoint 1 OUT.
        let mut bulk = device.bulk_out_endpoint_handle(4);
        let mut interrupt = device.interrupt_out_endpoint_handle(2);

        let payload: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        bulk.submit(payload.clone()).unwrap();
        let (submit, data) = peer.submit().await;
        assert_eq!((submit.direction, submit.ep), (Direction::Out, 2));
        assert_eq!(submit.transfer_buffer_length, 1000);
        assert_eq!(data, payload);
        peer.accept(submit.seqnum, 1000).await;
        let completion = bulk.next_completion().await.unwrap();
        assert!(matches!(completion.status, OutTrbProcessingStatus::Success));
        assert_eq!(completion.length, 1000);

        // The device may accept less than it was given, and the transfer can be empty.
        interrupt.submit(vec![0xaa; 64]).unwrap();
        let (submit, data) = peer.submit().await;
        assert_eq!((submit.direction, submit.ep), (Direction::Out, 1));
        assert_eq!(data, [0xaa; 64]);
        peer.accept(submit.seqnum, 60).await;
        assert_eq!(interrupt.next_completion().await.unwrap().length, 60);

        interrupt.submit(Vec::new()).unwrap();
        let (submit, data) = peer.submit().await;
        assert_eq!(submit.transfer_buffer_length, 0);
        assert!(data.is_empty());
        peer.accept(submit.seqnum, 0).await;
        assert_eq!(interrupt.next_completion().await.unwrap().length, 0);
    }

    #[tokio::test]
    async fn a_cancelled_transfer_is_unlinked_and_the_endpoint_works_again() {
        let (device, mut peer, detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        // The endpoint waits and is stopped, which drops the wait.
        timeout(Duration::from_millis(50), interrupt.next_completion())
            .await
            .unwrap_err();

        let cancelled = tokio::spawn(async move {
            interrupt.cancel().await.unwrap();
            interrupt
        });
        let unlink = peer.unlink().await;
        assert_eq!(unlink.unlink_seqnum, submit.seqnum);
        assert_ne!(unlink.seqnum, submit.seqnum);
        assert!(!cancelled.is_finished(), "cancel waits for the RET_UNLINK");
        peer.answer_unlink(unlink.seqnum, STATUS_CANCELLED).await;
        let mut interrupt = cancelled.await.unwrap();

        interrupt.submit(8).unwrap();
        let (next, _) = peer.submit().await;
        assert!(next.seqnum > unlink.seqnum);
        peer.complete(next.seqnum, 0, &[5]).await;
        assert_eq!(interrupt.next_completion().await.unwrap().data, [5]);
        assert!(!detach.is_cancelled());
    }

    #[tokio::test]
    async fn a_control_request_is_cancelled_like_any_transfer() {
        let (device, mut peer, _detach) = device();
        let mut control = device.control_endpoint_handle();

        control.submit_control_request(get_descriptor(18)).unwrap();
        let (submit, _) = peer.submit().await;
        let cancelled = tokio::spawn(async move { control.cancel().await.unwrap() });
        let unlink = peer.unlink().await;
        assert_eq!(unlink.unlink_seqnum, submit.seqnum);
        peer.answer_unlink(unlink.seqnum, STATUS_CANCELLED).await;
        cancelled.await.unwrap();
    }

    #[tokio::test]
    async fn the_result_of_a_completed_transfer_is_thrown_away_by_a_cancel() {
        let (device, mut peer, _detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        peer.complete(submit.seqnum, 0, &[1, 2]).await;
        // Give the task the time to hand the completion to the endpoint.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // No unlink goes out for a transfer that completed.
        interrupt.cancel().await.unwrap();
        interrupt.submit(8).unwrap();
        let (next, _) = peer.submit().await;
        peer.complete(next.seqnum, 0, &[3]).await;
        assert_eq!(interrupt.next_completion().await.unwrap().data, [3]);
    }

    #[tokio::test]
    async fn a_ret_submit_before_the_ret_unlink_wins_the_race() {
        let (device, mut peer, detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        let cancelled = tokio::spawn(async move {
            interrupt.cancel().await.unwrap();
            interrupt
        });
        let unlink = peer.unlink().await;

        // The report left the device before it saw the unlink. The unlink failed, and the data
        // of the report is dropped.
        peer.complete(submit.seqnum, 0, &[1, 2, 3, 4]).await;
        peer.answer_unlink(unlink.seqnum, 0).await;
        let mut interrupt = cancelled.await.unwrap();

        interrupt.submit(8).unwrap();
        let (next, _) = peer.submit().await;
        peer.complete(next.seqnum, 0, &[9]).await;
        assert_eq!(interrupt.next_completion().await.unwrap().data, [9]);
        assert!(!detach.is_cancelled());
    }

    #[tokio::test]
    async fn a_ret_submit_after_a_failed_unlink_is_read_and_dropped() {
        let (device, mut peer, detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        let cancelled = tokio::spawn(async move {
            interrupt.cancel().await.unwrap();
            interrupt
        });
        let unlink = peer.unlink().await;

        // The unlink did not cancel the transfer, and its RET_SUBMIT with data comes later.
        peer.answer_unlink(unlink.seqnum, 0).await;
        let mut interrupt = cancelled.await.unwrap();
        interrupt.submit(8).unwrap();
        let (next, _) = peer.submit().await;
        peer.complete(submit.seqnum, 0, &[1, 2, 3, 4]).await;
        peer.complete(next.seqnum, 0, &[9]).await;

        let completion = interrupt.next_completion().await.unwrap();
        assert_eq!(completion.data, [9], "the old report is not the new one");
        assert!(!detach.is_cancelled());
    }

    #[tokio::test]
    async fn a_device_that_does_not_answer_the_unlink_is_detached() {
        let (device, mut peer, detach) = device_at(Speed::High);
        let device = device.with_unlink_timeout(Duration::from_millis(100));
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        interrupt.cancel().await.unwrap();

        assert_eq!(peer.unlink().await.unlink_seqnum, submit.seqnum);
        assert!(detach.is_cancelled());
        assert!(peer.closed().await, "the socket is shut down");
    }

    #[tokio::test]
    async fn dropping_an_endpoint_with_a_transfer_in_flight_unlinks_it() {
        let (device, mut peer, _detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);

        interrupt.submit(8).unwrap();
        let (submit, _) = peer.submit().await;
        drop(interrupt);

        let unlink = peer.unlink().await;
        assert_eq!(unlink.unlink_seqnum, submit.seqnum);
        peer.answer_unlink(unlink.seqnum, STATUS_CANCELLED).await;
    }

    #[tokio::test]
    async fn the_end_of_the_stream_is_a_disconnect() {
        let (device, peer, detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);
        let mut control = device.control_endpoint_handle();
        let mut bulk_out = device.bulk_out_endpoint_handle(4);

        interrupt.submit(8).unwrap();
        control.submit_control_request(get_descriptor(18)).unwrap();
        drop(peer);

        assert!(matches!(
            interrupt.next_completion().await.unwrap().status,
            InTrbProcessingStatus::Disconnect
        ));
        assert!(matches!(
            control.next_completion().await.unwrap(),
            ControlRequestProcessingResult::Disconnect
        ));
        assert!(detach.is_cancelled());

        // The endpoints of a device that is gone still take transfers, and report the loss.
        bulk_out.submit(vec![1]).unwrap();
        assert!(matches!(
            bulk_out.next_completion().await.unwrap().status,
            OutTrbProcessingStatus::Disconnect
        ));
        interrupt.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn broken_framing_detaches_the_device() {
        for garbage in [
            // A command that does not exist.
            Header::CmdSubmit(CmdSubmit {
                seqnum: 1,
                direction: Direction::In,
                ep: 0,
                transfer_buffer_length: 0,
                setup: [0; 8],
            })
            .encode()
            .to_vec(),
            // A completion that answers nothing.
            Header::RetSubmit(RetSubmit {
                seqnum: 77,
                status: 0,
                actual_length: 0,
            })
            .encode()
            .to_vec(),
            Header::RetUnlink(RetUnlink {
                seqnum: 77,
                status: 0,
            })
            .encode()
            .to_vec(),
            vec![0xff; HEADER_LEN],
        ] {
            let (device, mut peer, detach) = device();
            let mut interrupt = device.interrupt_in_endpoint_handle(3);
            interrupt.submit(8).unwrap();
            let _ = peer.submit().await;

            peer.0.write_all(&garbage).await.unwrap();

            assert!(matches!(
                interrupt.next_completion().await.unwrap().status,
                InTrbProcessingStatus::Disconnect
            ));
            assert!(detach.is_cancelled());
            assert!(peer.closed().await);
        }
    }

    #[tokio::test]
    async fn a_completion_that_returns_more_than_was_asked_detaches_the_device() {
        let (device, mut peer, detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);
        interrupt.submit(2).unwrap();
        let (submit, _) = peer.submit().await;

        peer.complete(submit.seqnum, 0, &[1, 2, 3]).await;

        assert!(matches!(
            interrupt.next_completion().await.unwrap().status,
            InTrbProcessingStatus::Disconnect
        ));
        assert!(detach.is_cancelled());
    }

    #[tokio::test]
    async fn data_that_arrives_in_pieces_is_put_together() {
        let (device, mut peer, _detach) = device();
        let mut interrupt = device.interrupt_in_endpoint_handle(3);
        interrupt.submit(64).unwrap();
        let (submit, _) = peer.submit().await;

        let header = Header::RetSubmit(RetSubmit {
            seqnum: submit.seqnum,
            status: 0,
            actual_length: 64,
        })
        .encode();
        let bytes: Vec<u8> = header.into_iter().chain(0..64).collect();
        for piece in bytes.chunks(7) {
            peer.0.write_all(piece).await.unwrap();
            peer.0.flush().await.unwrap();
        }

        let data = interrupt.next_completion().await.unwrap().data;
        assert_eq!(data, (0..64).collect::<Vec<u8>>());
    }

    #[tokio::test]
    async fn the_device_in_the_tree_follows_the_socket() {
        let (event_sender, _interrupter) = MockInterrupter::new();
        let port_array: PortArray<ControllerDevice> =
            PortArray::new(event_sender, runtime::Handle::current());
        let control = port_array.create_hotplug_control();

        let attach = async |number: u32, name: Option<&str>| {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let detach = CancellationToken::new();
            let device = ExternalDevice::start(
                ours,
                Speed::Full,
                number,
                name.map(str::to_string),
                detach.clone(),
                &runtime::Handle::current(),
            );
            let mut complete =
                CompleteRealDeviceImpl::new(DeviceId::External(number), AnyDevice::new(device));
            complete.cancel = detach;
            let token = complete.detach_token();
            let response = control.attach_at("5".parse().unwrap(), complete).await;
            assert_eq!(response, Response::SuccessfulOperation);
            (Peer(theirs), token)
        };
        let paths = async || {
            control
                .tree()
                .await
                .into_iter()
                .map(|node| (node.path.to_string(), node.id))
                .collect::<Vec<_>>()
        };

        // The peer closes the socket: the device leaves the tree by itself.
        let (peer, token) = attach(1001, Some("Keys")).await;
        assert_eq!(paths().await, [("5".to_string(), DeviceId::External(1001))]);
        assert_eq!(describe(1001), "external Keys");
        drop(peer);
        eventually(|| token.is_cancelled()).await;
        for _ in 0..500 {
            if paths().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(paths().await.is_empty());
        assert_eq!(describe(1001), "external #1001", "the name is forgotten");

        // A detach command shuts the socket down.
        let (mut peer, token) = attach(1002, None).await;
        assert_eq!(paths().await.len(), 1);
        assert_eq!(
            control.detach_at("5".parse().unwrap()).await,
            Response::SuccessfulOperation
        );
        assert!(token.is_cancelled());
        assert!(peer.closed().await, "the peer sees the end of the stream");
        assert!(paths().await.is_empty());
    }
}
