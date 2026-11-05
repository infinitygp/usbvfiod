use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{
    device::{
        bus::BusDeviceRef,
        pcap::EndpointPcapMeta,
        xhci::{
            endpoint::{EndpointSender, EndpointWorker},
            endpoint_handle::{
                ControlEndpointHandle, EndpointHandle, TdBasedInEndpointHandle,
                TdBasedOutEndpointHandle,
            },
            hotplug_endpoint_handle::HotplugEndpointHandleImpl,
            interrupter::EventSender,
            port::DeviceRetriever,
            real_device::{CompleteRealDevice, RealDevice},
            slot_manager::{EndpointContext, EndpointType},
        },
    },
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct EndpointLauncher<CRD: CompleteRealDevice> {
    request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
    device_retriever: DeviceRetriever<CRD>,
    async_runtime: runtime::Handle,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
}

#[derive(Debug)]
pub struct LaunchRequest {
    pub slot_id: u8,
    pub endpoint_id: u8,
    pub root_hub_port: u8,
    /// The hub ports from the root hub to the device, as in the slot context.
    pub route_string: u32,
    pub endpoint_context: EndpointContext,
    pub responder: oneshot::Sender<EndpointSender>,
}

#[derive(Debug, Clone)]
pub struct LaunchRequester {
    msg_send: mpsc::UnboundedSender<LaunchRequest>,
}

#[derive(Debug)]
struct LaunchArgs {
    slot_id: u8,
    endpoint_id: u8,
    endpoint_context: EndpointContext,
    detach_token: CancellationToken,
}

impl LaunchRequester {
    pub async fn request_launch(
        &self,
        slot_id: u8,
        endpoint_id: u8,
        root_hub_port: u8,
        route_string: u32,
        endpoint_context: EndpointContext,
    ) -> anyhow::Result<EndpointSender> {
        let (send, recv) = oneshot::channel();
        let launch_request = LaunchRequest {
            slot_id,
            endpoint_id,
            root_hub_port,
            route_string,
            endpoint_context,
            responder: send,
        };
        self.msg_send.send(launch_request)?;
        let ep_sender = recv.await?;

        Ok(ep_sender)
    }
}

impl<CRD: CompleteRealDevice> EndpointLauncher<CRD> {
    pub fn start(
        device_retriever: DeviceRetriever<CRD>,
        async_runtime: runtime::Handle,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> LaunchRequester {
        let (send, recv) = mpsc::unbounded_channel();
        let launcher = Self {
            request_recv: recv,
            device_retriever,
            async_runtime: async_runtime.clone(),
            dma_bus,
            event_sender,
        };
        async_runtime.spawn(launcher.run());

        LaunchRequester { msg_send: send }
    }

    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => {
                info!("EndpointLauncher stopped {err}");
            }
        }
    }

    // function only returns on error, but cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            let request = self.next_msg().await?;
            debug!(
                "endpoint launch request for slot {} endpoint {} with device at root hub port {} and route string {:#07x}",
                request.slot_id, request.endpoint_id, request.root_hub_port, request.route_string
            );

            let device = self
                .device_retriever
                .get_device(request.root_hub_port, request.route_string)
                .await?;
            let endpoint_type = request.endpoint_context.get_endpoint_type();
            debug!("endpoint context specifies endpoint type {endpoint_type:?}");

            let endpoint_sender = match device {
                Some(device) => {
                    let pcap_usb_bus_number = match device.realdevice_ref().speed() {
                        Some(speed) if speed.is_usb2_speed() => 2,
                        Some(_) => 3,
                        None => 0,
                    };
                    let launch_args = LaunchArgs {
                        slot_id: request.slot_id,
                        endpoint_id: request.endpoint_id,
                        endpoint_context: request.endpoint_context,
                        detach_token: device.detach_token(),
                    };

                    match endpoint_type {
                    EndpointType::Control => {
                        let pcap_meta = EndpointPcapMeta::control(
                            pcap_usb_bus_number,
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device.realdevice_ref().control_endpoint_handle();
                        self.launch_helper(
                            ControlEndpointHandle::new,
                            launch_args,
                            pcap_meta,
                            real_endpoint,
                        )
                    }
                    EndpointType::BulkIn => {
                        let pcap_meta = EndpointPcapMeta::bulk(
                            pcap_usb_bus_number,
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .bulk_in_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            TdBasedInEndpointHandle::new,
                            launch_args,
                            pcap_meta,
                            real_endpoint,
                        )
                    }
                    EndpointType::BulkOut => {
                        let pcap_meta = EndpointPcapMeta::bulk(
                            pcap_usb_bus_number,
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .bulk_out_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            TdBasedOutEndpointHandle::new,
                            launch_args,
                            pcap_meta,
                            real_endpoint,
                        )
                    }
                    EndpointType::InterruptIn => {
                        let pcap_meta = EndpointPcapMeta::interrupt(
                            pcap_usb_bus_number,
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .interrupt_in_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            TdBasedInEndpointHandle::new,
                            launch_args,
                            pcap_meta,
                            real_endpoint,
                        )
                    }
                    EndpointType::InterruptOut => {
                        let pcap_meta = EndpointPcapMeta::interrupt(
                            pcap_usb_bus_number,
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .interrupt_out_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            TdBasedOutEndpointHandle::new,
                            launch_args,
                            pcap_meta,
                            real_endpoint,
                        )
                    }
                    _ => panic!(
                        "the slot should early-reject configure endpoint commands with unsupported endpoint types; this path is hit if system software modifies the field after we check and before we use it here"
                    ),
                    }
                }
                // unlikely edge case: The device was very recently detached, we are now handling an address device/configure endpoint command
                // the endpoint does not depend on the real device, so we need the address device/configure endpoint to succeed (while also not
                // creating an invalid state)
                None => {
                    debug!("Could not get real device, using dummy endpoint handle");
                    let hotplug_endpoint_handle = HotplugEndpointHandleImpl::dummy(
                        request.slot_id,
                        request.endpoint_id,
                        self.event_sender.clone(),
                    );

                    EndpointWorker::launch(
                        &self.async_runtime,
                        self.dma_bus.clone(),
                        hotplug_endpoint_handle,
                        request.endpoint_context,
                    )
                }
            };

            request.responder.send_anyhow(endpoint_sender)?;
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<LaunchRequest> {
        self.request_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("channel should never close"))
    }

    fn launch_helper<RealEndpoint, Endpoint, EndpointConstructor>(
        &self,
        constructor: EndpointConstructor,
        launch_args: LaunchArgs,
        pcap_meta: EndpointPcapMeta,
        real_endpoint: RealEndpoint,
    ) -> EndpointSender
    where
        Endpoint: EndpointHandle,
        EndpointConstructor:
            FnOnce(u8, u8, EndpointPcapMeta, RealEndpoint, BusDeviceRef, EventSender) -> Endpoint,
    {
        let endpoint_handle = constructor(
            launch_args.slot_id,
            launch_args.endpoint_id,
            pcap_meta,
            real_endpoint,
            self.dma_bus.clone(),
            self.event_sender.clone(),
        );
        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
            launch_args.slot_id,
            launch_args.endpoint_id,
            endpoint_handle,
            self.event_sender.clone(),
            launch_args.detach_token,
            &self.async_runtime,
        );

        EndpointWorker::launch(
            &self.async_runtime,
            self.dma_bus.clone(),
            hotplug_endpoint_handle,
            launch_args.endpoint_context,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tokio::runtime::Handle;
    use usbvfiod::hotplug_protocol::response::Response;

    use super::*;
    use crate::{
        device::{
            bus::{testutils::TestBusDevice, BusDevice, Request, RequestSize},
            xhci::{
                hub_ports::{HubHandle, HubShared},
                interrupter::tests::testutils::MockInterrupter,
                port::PortArray,
                real_device::{
                    tests::testutils::MockRealDeviceWithSpeed, CompleteRealDeviceImpl, Speed,
                },
            },
        },
        dynamic_bus::DynamicBus,
    };

    /// A device that counts how often the controller asks it for its control endpoint, which is
    /// what a launched control endpoint does.
    #[derive(Debug)]
    struct CountingDevice {
        speed: Speed,
        control_endpoints: Arc<AtomicUsize>,
    }

    type Mock = MockRealDeviceWithSpeed;

    impl RealDevice for CountingDevice {
        type RCEH = <Mock as RealDevice>::RCEH;
        type RBIEH = <Mock as RealDevice>::RBIEH;
        type RBOEH = <Mock as RealDevice>::RBOEH;
        type RIIEH = <Mock as RealDevice>::RIIEH;
        type RIOEH = <Mock as RealDevice>::RIOEH;

        fn speed(&self) -> Option<Speed> {
            Some(self.speed)
        }

        fn control_endpoint_handle(&self) -> Self::RCEH {
            self.control_endpoints.fetch_add(1, Ordering::SeqCst);
            Mock::from(self.speed).control_endpoint_handle()
        }

        fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
            Mock::from(self.speed).bulk_in_endpoint_handle(endpoint_id)
        }

        fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
            Mock::from(self.speed).bulk_out_endpoint_handle(endpoint_id)
        }

        fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
            Mock::from(self.speed).interrupt_in_endpoint_handle(endpoint_id)
        }

        fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
            Mock::from(self.speed).interrupt_out_endpoint_handle(endpoint_id)
        }
    }

    type Device = CompleteRealDeviceImpl<CountingDevice, (u8, u8)>;

    const CONTEXT: u64 = 0x100;
    /// EP Type 4: control.
    const CONTROL_ENDPOINT_TYPE: u64 = 4 << 3;

    fn device(id: u8, speed: Speed) -> (Device, Arc<AtomicUsize>) {
        let control_endpoints = Arc::new(AtomicUsize::new(0));
        let device = CompleteRealDeviceImpl::new(
            (id, id),
            CountingDevice {
                speed,
                control_endpoints: control_endpoints.clone(),
            },
        );
        (device, control_endpoints)
    }

    fn hub(id: u8) -> (Device, Arc<AtomicUsize>) {
        let (device, counter) = device(id, Speed::High);
        let shared = Arc::new(HubShared::new(4, Speed::High));
        (device.with_hub(HubHandle::new(shared)), counter)
    }

    #[tokio::test]
    async fn an_endpoint_launches_on_the_device_that_the_route_string_names() {
        let (event_sender, _interrupter) = MockInterrupter::new();
        let port_array: PortArray<Device> = PortArray::new(event_sender.clone(), Handle::current());
        let control = port_array.create_hotplug_control();
        let dma_bus = Arc::new(DynamicBus::new());
        dma_bus
            .add(0, Arc::new(TestBusDevice::new(&[0u8; 0x1000])))
            .unwrap();
        dma_bus.write(
            Request::new(CONTEXT + 4, RequestSize::Size1),
            CONTROL_ENDPOINT_TYPE,
        );
        let launcher = EndpointLauncher::start(
            port_array.create_device_retriever(),
            Handle::current(),
            dma_bus.clone(),
            event_sender,
        );

        // Two hubs in a row, with a device on the second and one on the first, and a device on
        // another root port.
        let mut counters = Vec::new();
        for (path, (device, counter)) in [
            ("5", hub(1)),
            ("5.1", hub(2)),
            ("5.1.4", device(3, Speed::Full)),
            ("5.2", device(4, Speed::Full)),
            ("6", device(5, Speed::Full)),
        ] {
            assert_eq!(
                control.attach_at(path.parse().unwrap(), device).await,
                Response::SuccessfulOperation
            );
            counters.push(counter);
        }
        let launches = || {
            counters
                .iter()
                .map(|counter| counter.load(Ordering::SeqCst))
                .collect::<Vec<_>>()
        };
        let launch = async |root_port, route_string| {
            launcher
                .request_launch(
                    1,
                    1,
                    root_port,
                    route_string,
                    EndpointContext::new(CONTEXT, dma_bus.clone()),
                )
                .await
                .unwrap()
                .terminate()
                .await
                .unwrap();
        };

        launch(5, 0).await;
        assert_eq!(launches(), [1, 0, 0, 0, 0], "the hub on the root port");
        launch(5, 0x1).await;
        assert_eq!(launches(), [1, 1, 0, 0, 0], "the hub on its first port");
        launch(5, 0x41).await;
        assert_eq!(launches(), [1, 1, 1, 0, 0], "port 4 of that hub");
        launch(5, 0x2).await;
        assert_eq!(launches(), [1, 1, 1, 1, 0], "port 2 of the first hub");
        launch(6, 0).await;
        assert_eq!(
            launches(),
            [1, 1, 1, 1, 1],
            "the device on another root port"
        );

        // No device is there: the endpoint gets a dummy handle, and no device is asked.
        launch(5, 0x3).await;
        launch(5, 0x42).await;
        launch(7, 0).await;
        assert_eq!(launches(), [1, 1, 1, 1, 1]);
    }
}
