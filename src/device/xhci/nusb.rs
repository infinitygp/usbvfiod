use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Error};
use nusb::{
    transfer::{
        Buffer, Bulk, BulkOrInterrupt, Completion, ControlIn, ControlOut, ControlType,
        EndpointDirection, EndpointType, In, Interrupt, Out, Recipient, TransferError,
    },
    Endpoint, ErrorKind, Interface, MaybeFuture,
};
use tokio::{
    runtime, select,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
        OutTrbProcessingStatus, RealControlEndpointHandle, RealInEndpointHandle,
        RealOutEndpointHandle,
    },
    usbrequest::UsbRequest,
};

use super::real_endpoint_handle::OutTrbProcessingResult;

struct NusbDeviceWrapper {
    device: nusb::Device,
    interfaces: Vec<Interface>,
}

impl Debug for NusbDeviceWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The active configuration is either cached or not available
        // for unconfigured devices. There is no I/O for this.
        f.debug_struct("NusbDeviceWrapper")
            .field("device", &self.device.active_configuration())
            .finish()
    }
}

impl TryFrom<nusb::Device> for NusbDeviceWrapper {
    type Error = Error;

    fn try_from(device: nusb::Device) -> Result<Self, Error> {
        // Claim all interfaces
        let mut interfaces = vec![];
        let desc = device.active_configuration()?;
        for interface in desc.interfaces() {
            let interface_number = interface.interface_number();
            debug!("Claiming interface {}", interface_number);
            interfaces.push(device.detach_and_claim_interface(interface_number).wait()?);
        }

        CLAIMED_HOST_DEVICES.fetch_add(1, Ordering::SeqCst);
        Ok(Self { device, interfaces })
    }
}

/// The host devices whose interfaces usbvfiod holds.
static CLAIMED_HOST_DEVICES: AtomicUsize = AtomicUsize::new(0);

/// How many host devices usbvfiod holds: their kernel drivers are detached until they are
/// dropped.
pub fn claimed_host_devices() -> usize {
    CLAIMED_HOST_DEVICES.load(Ordering::SeqCst)
}

impl Drop for NusbDeviceWrapper {
    fn drop(&mut self) {
        // Releasing an interface gives it back to the kernel driver that
        // detach_and_claim_interface took it from. An endpoint's transfers that were cancelled
        // hold their interface until the kernel returns them, a moment later.
        self.interfaces.clear();
        CLAIMED_HOST_DEVICES.fetch_sub(1, Ordering::SeqCst);
    }
}

impl NusbDeviceWrapper {
    fn get_interface_number_containing_endpoint(&self, endpoint_id: u8) -> Option<usize> {
        self.interfaces.iter().position(|interface| {
            interface
                .descriptor()
                .unwrap()
                .endpoints()
                .any(|ep| ep.address() == endpoint_id)
        })
    }

    fn open_endpoint<EpType: EndpointType, Dir: EndpointDirection>(
        &self,
        endpoint_id: u8,
    ) -> Result<Endpoint<EpType, Dir>, Error> {
        let endpoint_address = endpoint_id_to_address(endpoint_id);
        let interface_num = self
            .get_interface_number_containing_endpoint(endpoint_address)
            .ok_or_else(|| anyhow!("Endpoint with id {endpoint_id} is not part of an interface"))?;
        let endpoint = self.interfaces[interface_num].endpoint(endpoint_address)?;

        Ok(endpoint)
    }

    async fn set_alt_setting(
        &self,
        interface_number: u8,
        alt_setting: u8,
    ) -> ControlRequestProcessingResult {
        let Some(interface) = self
            .interfaces
            .iter()
            .find(|interface| interface.interface_number() == interface_number)
        else {
            warn!("SET_INTERFACE for unknown interface {interface_number}");
            return ControlRequestProcessingResult::Stall;
        };

        match interface.set_alt_setting(alt_setting).await {
            Ok(()) => ControlRequestProcessingResult::SuccessfulControlOut,
            Err(err) => {
                warn!("Failed to set alternate setting {alt_setting} of interface {interface_number}: {err}");
                match err.kind() {
                    // A real device would stall a request for a nonexistent alternate setting.
                    ErrorKind::NotFound => ControlRequestProcessingResult::Stall,
                    ErrorKind::Disconnected => ControlRequestProcessingResult::Disconnect,
                    _ => ControlRequestProcessingResult::TransactionError,
                }
            }
        }
    }
}

const fn endpoint_id_to_address(endpoint_id: u8) -> u8 {
    // NOTE: This only applies to normal endpoints!
    // `endpoint_id / 2` yields the USB endpoint number, while the low bit of the
    // xHCI endpoint id distinguishes OUT (even) from IN (odd). Rotating right by
    // one moves that direction bit into the USB IN position (0x80) and leaves the
    // endpoint number in the low bits.
    endpoint_id.rotate_right(1)
}

#[derive(Debug)]
pub struct NusbRealDevice {
    device_wrapper: Arc<NusbDeviceWrapper>,
    async_runtime: runtime::Handle,
}

impl NusbRealDevice {
    pub fn try_new(device: nusb::Device, async_runtime: runtime::Handle) -> Result<Self, Error> {
        let device_wrapper = NusbDeviceWrapper::try_from(device)?;

        Ok(Self {
            device_wrapper: Arc::new(device_wrapper),
            async_runtime,
        })
    }
}

impl RealDevice for NusbRealDevice {
    type RCEH = ControlEndpointHandle;
    type RBIEH = NormalEndpointHandle<Bulk, In>;
    type RBOEH = NormalEndpointHandle<Bulk, Out>;
    type RIIEH = NormalEndpointHandle<Interrupt, In>;
    type RIOEH = NormalEndpointHandle<Interrupt, Out>;

    fn speed(&self) -> Option<super::real_device::Speed> {
        self.device_wrapper.device.speed().map(|speed| speed.into())
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        ControlEndpointHandle::new(self.device_wrapper.clone(), &self.async_runtime)
    }

    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }

    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }

    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }

    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }
}

/// A request for the worker, with its number.
type NumberedRequest = (u64, UsbRequest);

/// The worker's answer to a request with its number; none for a request it gave up.
type NumberedAnswer = (u64, Option<ControlRequestProcessingResult>);

#[derive(Debug)]
pub struct ControlEndpointHandle {
    // signal worker to stop
    cancel: CancellationToken,
    request_submitter: mpsc::UnboundedSender<NumberedRequest>,
    response_receiver: mpsc::UnboundedReceiver<NumberedAnswer>,
    /// The worker gives up the requests up to this number.
    abort: watch::Sender<u64>,
    /// The number of the last request submitted.
    submitted: u64,
    /// The number of the last request the worker answered or gave up.
    answered: u64,
}

impl Drop for ControlEndpointHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl RealControlEndpointHandle for ControlEndpointHandle {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.submitted += 1;
        self.request_submitter.send((self.submitted, request))?;

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            loop {
                let Some((number, result)) = self.response_receiver.recv().await else {
                    // background worker is dead and has dropped the response sender
                    return Ok(ControlRequestProcessingResult::TransactionError);
                };
                self.answered = number;
                // The answer to a request that was given up is not the answer to this one.
                if number == self.submitted {
                    return Ok(result.unwrap_or(ControlRequestProcessingResult::TransactionError));
                }
            }
        })
    }
}

impl BaseEndpointHandle for ControlEndpointHandle {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    /// The guest stopped the endpoint: the transfer on the device is cancelled, as an xHCI
    /// controller aborts it.
    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            if self.answered < self.submitted {
                self.abort.send_replace(self.submitted);
                // The device is done with the request once the worker answered or gave it up.
                while let Some((number, _)) = self.response_receiver.recv().await {
                    self.answered = number;
                    if number >= self.submitted {
                        break;
                    }
                }
            }
            Ok(())
        })
    }
}

impl ControlEndpointHandle {
    fn new(device_wrapper: Arc<NusbDeviceWrapper>, async_runtime: &runtime::Handle) -> Self {
        Self::with_performer(
            move |request| perform_control_request(device_wrapper.clone(), request),
            async_runtime,
        )
    }

    /// A handle whose worker carries out a request with `perform`.
    fn with_performer<F, Fut>(perform: F, async_runtime: &runtime::Handle) -> Self
    where
        F: FnMut(UsbRequest) -> Fut + Send + 'static,
        Fut: Future<Output = ControlRequestProcessingResult> + Send + 'static,
    {
        let (request_submitter, request_receiver) = mpsc::unbounded_channel();
        let (response_submitter, response_receiver) = mpsc::unbounded_channel();
        let (abort, abort_receiver) = watch::channel(0);
        let cancel = CancellationToken::new();

        let worker = control_endpoint_worker(
            perform,
            request_receiver,
            response_submitter,
            abort_receiver,
        );
        let stop = cancel.clone();
        async_runtime.spawn(async move {
            select! {
                () = worker => {},
                () = stop.cancelled() => {},
            }
        });

        Self {
            cancel,
            request_submitter,
            response_receiver,
            abort,
            submitted: 0,
            answered: 0,
        }
    }
}

/// Carry out the requests one after the other, until the handle goes. A request that the handle
/// gives up is dropped, which cancels its transfer on the device.
async fn control_endpoint_worker<F, Fut>(
    mut perform: F,
    mut request_receiver: mpsc::UnboundedReceiver<NumberedRequest>,
    response_submitter: mpsc::UnboundedSender<NumberedAnswer>,
    mut abort: watch::Receiver<u64>,
) where
    F: FnMut(UsbRequest) -> Fut,
    Fut: Future<Output = ControlRequestProcessingResult>,
{
    while let Some((number, request)) = request_receiver.recv().await {
        let result = if *abort.borrow() >= number {
            None
        } else {
            select! {
                biased;
                result = perform(request) => Some(result),
                _ = abort.wait_for(|&given_up| given_up >= number) => None,
            }
        };
        if response_submitter.send((number, result)).is_err() {
            return;
        }
    }
}

// bmRequestType of a standard request addressed to an interface (host-to-device).
const REQUEST_TYPE_STANDARD_INTERFACE_OUT: u8 = 0x01;
const REQUEST_SET_INTERFACE: u8 = 0x0b;

/// An xHCI controller does not end a transfer by time: the guest's driver stops the endpoint
/// when it gives up, which cancels the transfer. nusb takes this as a day.
const NO_TIMEOUT: Duration = Duration::MAX;

async fn perform_control_request(
    device_wrapper: Arc<NusbDeviceWrapper>,
    request: UsbRequest,
) -> ControlRequestProcessingResult {
    let device = &device_wrapper.device;
    let (recipient, control_type) = extract_recipient_and_type(request.request_type);
    let is_out_request = request.request_type & 0x80 == 0;
    let is_set_interface = request.request_type == REQUEST_TYPE_STANDARD_INTERFACE_OUT
        && request.request == REQUEST_SET_INTERFACE;

    match is_out_request {
        // SET_INTERFACE must not be sent as a plain control transfer:
        // neither the host kernel nor nusb would learn about the new
        // alternate setting and its endpoints.
        true if is_set_interface => {
            // wIndex holds the interface number and wValue the alternate setting.
            device_wrapper
                .set_alt_setting(request.index as u8, request.value as u8)
                .await
        }
        true => {
            let control = ControlOut {
                control_type,
                recipient,
                request: request.request,
                value: request.value,
                index: request.index,
                data: &request.data,
            };
            match device.control_out(control, NO_TIMEOUT).await {
                Ok(_) => ControlRequestProcessingResult::SuccessfulControlOut,
                Err(err) => map_error(err),
            }
        }
        false => {
            let length = match u16::try_from(request.length) {
                Ok(length) => length,
                Err(_) => {
                    warn!("Control Transfer Request size larger than the nusb provided 16bit field size (data loss).");
                    request.length as u16
                }
            };
            let control = ControlIn {
                control_type,
                recipient,
                request: request.request,
                value: request.value,
                index: request.index,
                length,
            };
            match device.control_in(control, NO_TIMEOUT).await {
                Ok(data) => ControlRequestProcessingResult::SuccessfulControlIn(data),
                Err(err) => map_error(err),
            }
        }
    }
}

const fn map_error(error: TransferError) -> ControlRequestProcessingResult {
    match error {
        TransferError::Cancelled => ControlRequestProcessingResult::TransactionError,
        TransferError::Stall => ControlRequestProcessingResult::Stall,
        TransferError::Disconnected => ControlRequestProcessingResult::Disconnect,
        TransferError::Fault => ControlRequestProcessingResult::TransactionError,
        TransferError::InvalidArgument => ControlRequestProcessingResult::TransactionError,
        TransferError::Unknown(_) => ControlRequestProcessingResult::TransactionError,
    }
}

fn extract_recipient_and_type(request_type: u8) -> (Recipient, ControlType) {
    let recipient = match request_type & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        val => panic!("invalid recipient {val}"),
    };
    let control_type = match (request_type >> 5) & 0x3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        val => panic!("invalid type {val}"),
    };
    (recipient, control_type)
}

pub struct NormalEndpointHandle<EpType: EndpointType + 'static, Dir: EndpointDirection + 'static> {
    id: u8,
    // Before the device: an endpoint holds its interface, which the device releases when it goes.
    endpoint: Option<Endpoint<EpType, Dir>>,
    device_wrapper: Arc<NusbDeviceWrapper>,
}

impl<EpType: EndpointType, Dir: EndpointDirection> NormalEndpointHandle<EpType, Dir> {
    const fn new(id: u8, device_wrapper: Arc<NusbDeviceWrapper>) -> Self {
        Self {
            id,
            endpoint: None,
            device_wrapper,
        }
    }
}

impl<EpType: EndpointType, Dir: EndpointDirection> Debug for NormalEndpointHandle<EpType, Dir> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("lol")
            //.field("device", &self.device.active_configuration())
            .finish()
    }
}

impl<EpType: EndpointType, Dir: EndpointDirection> NormalEndpointHandle<EpType, Dir> {
    fn endpoint(&mut self) -> &mut Endpoint<EpType, Dir> {
        match self.endpoint {
            Some(ref mut endpoint) => endpoint,
            None => {
                let ep: Endpoint<EpType, Dir> = self.device_wrapper.open_endpoint(self.id).expect("Failed to open endpoint on nusb device. We could handle this error and always return transaction errors. Panic is fine for now.");
                self.endpoint = Some(ep);
                self.endpoint.as_mut().unwrap()
            }
        }
    }
}

impl<EpType: BulkOrInterrupt> RealOutEndpointHandle for NormalEndpointHandle<EpType, Out> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<OutTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let buf = Buffer::from(data);
        self.endpoint().submit(buf);

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let completion = self.endpoint().next_complete().await;

            let status = map_status_out(completion.status);
            let length = completion.actual_len;

            Ok(OutTrbProcessingResult { status, length })
        })
    }
}

const fn map_status_out(nusb_status: Result<(), TransferError>) -> OutTrbProcessingStatus {
    match nusb_status {
        Ok(_) => OutTrbProcessingStatus::Success,
        Err(err) => match err {
            TransferError::Cancelled => OutTrbProcessingStatus::TransactionError,
            TransferError::Stall => OutTrbProcessingStatus::Stall,
            TransferError::Disconnected => OutTrbProcessingStatus::Disconnect,
            TransferError::Fault => OutTrbProcessingStatus::TransactionError,
            TransferError::InvalidArgument => OutTrbProcessingStatus::TransactionError,
            TransferError::Unknown(_) => OutTrbProcessingStatus::TransactionError,
        },
    }
}

impl<EpType: BulkOrInterrupt> RealInEndpointHandle for NormalEndpointHandle<EpType, In> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, len: usize) -> anyhow::Result<()> {
        let endpoint = self.endpoint();
        let request_len = determine_buffer_size(len, endpoint.max_packet_size());
        let buf = Buffer::new(request_len);
        endpoint.submit(buf);

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let Completion {
                buffer: data,
                actual_len: _,
                status,
            } = self.endpoint().next_complete().await;
            let data = data.into_vec();
            let status = map_status_in(status);

            Ok(InTrbProcessingResult { status, data })
        })
    }
}

const fn map_status_in(nusb_status: Result<(), TransferError>) -> InTrbProcessingStatus {
    match nusb_status {
        Ok(_) => InTrbProcessingStatus::Success,
        Err(err) => match err {
            TransferError::Cancelled => InTrbProcessingStatus::TransactionError,
            TransferError::Stall => InTrbProcessingStatus::Stall,
            TransferError::Disconnected => InTrbProcessingStatus::Disconnect,
            TransferError::Fault => InTrbProcessingStatus::TransactionError,
            TransferError::InvalidArgument => InTrbProcessingStatus::TransactionError,
            TransferError::Unknown(_) => InTrbProcessingStatus::TransactionError,
        },
    }
}

impl<EpType: BulkOrInterrupt, Dir: EndpointDirection> BaseEndpointHandle
    for NormalEndpointHandle<EpType, Dir>
{
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            let ep = self.endpoint();
            ep.cancel_all();

            // have to consume all cancelled TRBs (should be 0 or 1). A transfer that finished
            // before the cancel took effect loses its result here. The endpoint worker prefers a
            // completion that is there over a stop, which leaves only the moment between its
            // decision and the cancel.
            if ep.pending() > 1 {
                warn!("while cancelling: saw more than one pending TRB");
            }
            while ep.pending() > 0 {
                let _ = ep.next_complete().await;
            }

            Ok(())
        })
    }
}

const fn determine_buffer_size(guest_transfer_length: usize, max_packet_size: usize) -> usize {
    if guest_transfer_length <= max_packet_size {
        max_packet_size
    } else {
        guest_transfer_length.div_ceil(max_packet_size) * max_packet_size
    }
}

impl From<nusb::Speed> for Speed {
    fn from(value: nusb::Speed) -> Self {
        match value {
            nusb::Speed::Low => Self::Low,
            nusb::Speed::Full => Self::Full,
            nusb::Speed::High => Self::High,
            nusb::Speed::Super => Self::Super,
            nusb::Speed::SuperPlus => Self::SuperPlus,
            _ => todo!("new USB speed was added to non-exhaustive enum"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use tokio::time::timeout;

    use super::*;

    /// Sets its flag when the transfer it is part of is dropped, which cancels it on the device.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn request(value: u16) -> UsbRequest {
        UsbRequest {
            request_type: 0x80,
            request: 6,
            value,
            length: 255,
            ..UsbRequest::default()
        }
    }

    type Answer = Pin<Box<dyn Future<Output = ControlRequestProcessingResult> + Send>>;

    /// A device that never answers the request with wValue 0x0303 and answers others with the
    /// low byte of their wValue.
    fn hanging_device(cancelled: Arc<AtomicBool>) -> impl FnMut(UsbRequest) -> Answer {
        move |request| {
            let dropped = Dropped(cancelled.clone());
            Box::pin(async move {
                if request.value == 0x0303 {
                    let _dropped = dropped;
                    std::future::pending::<()>().await;
                }
                ControlRequestProcessingResult::SuccessfulControlIn(vec![request.value as u8])
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_control_transfer_lasts_until_the_guest_stops_the_endpoint_and_is_then_cancelled() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut handle = ControlEndpointHandle::with_performer(
            hanging_device(cancelled.clone()),
            &runtime::Handle::current(),
        );

        handle.submit_control_request(request(0x0303)).unwrap();
        // No time limit of its own: an hour later the transfer is still on.
        timeout(Duration::from_secs(3600), handle.next_completion())
            .await
            .unwrap_err();
        assert!(!cancelled.load(Ordering::SeqCst));

        handle.cancel().await.unwrap();
        assert!(cancelled.load(Ordering::SeqCst));

        // The next request gets its own answer.
        handle.submit_control_request(request(0x0300)).unwrap();
        assert!(matches!(
            handle.next_completion().await.unwrap(),
            ControlRequestProcessingResult::SuccessfulControlIn(data) if data == [0x00]
        ));
    }

    #[tokio::test]
    async fn the_answer_to_a_request_given_up_does_not_answer_the_next_one() {
        let mut handle = ControlEndpointHandle::with_performer(
            hanging_device(Arc::new(AtomicBool::new(false))),
            &runtime::Handle::current(),
        );

        // The device answers, but the guest stops the endpoint before it takes the answer.
        handle.submit_control_request(request(0x0301)).unwrap();
        handle.cancel().await.unwrap();

        handle.submit_control_request(request(0x0302)).unwrap();
        assert!(matches!(
            handle.next_completion().await.unwrap(),
            ControlRequestProcessingResult::SuccessfulControlIn(data) if data == [0x02]
        ));
    }

    #[tokio::test]
    async fn stopping_an_endpoint_without_a_request_on_it_does_nothing() {
        let mut handle = ControlEndpointHandle::with_performer(
            hanging_device(Arc::new(AtomicBool::new(false))),
            &runtime::Handle::current(),
        );
        handle.cancel().await.unwrap();

        handle.submit_control_request(request(0x0301)).unwrap();
        assert!(matches!(
            handle.next_completion().await.unwrap(),
            ControlRequestProcessingResult::SuccessfulControlIn(data) if data == [0x01]
        ));
    }
}
