use std::{array, sync::Arc};

use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use usbvfiod::hotplug_protocol::{port_path::PortPath, response::Response};

use crate::{
    device::{
        pci::constants::xhci::{offset, operational::portsc, MAX_PORTS, NUM_USB3_PORTS},
        xhci::{
            controller_reset::ResetSender,
            device_tree::{DeviceInstanceId, DeviceTree, Node},
            hub_ports::HubHandle,
            interrupter::EventSender,
            real_device::{CompleteRealDevice, RealDevice, Speed},
            registers::{PortpmscRegister, PortscRegister},
            trb::EventTrb,
        },
    },
    one_indexed_array::OneIndexed,
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct PortArray<CRD: CompleteRealDevice> {
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    portpmsc: Arc<OneIndexed<PortpmscRegister, { MAX_PORTS as usize }>>,
    pub msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> PortArray<CRD> {
    pub fn new(event_sender: EventSender, async_runtime: runtime::Handle) -> Self {
        let portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>> = Arc::new(
            array::from_fn(|index| {
                // SAFETY: port_id is capped at 255 according to spec
                let port_id = index as u8 + 1;
                PortscRegister::new(event_sender.clone(), Self::port_version(port_id), port_id)
            })
            .into(),
        );

        let portpmsc: Arc<OneIndexed<PortpmscRegister, { MAX_PORTS as usize }>> =
            Arc::new(array::from_fn(|_| PortpmscRegister::default()).into());

        let (msg_sender, msg_recv) = mpsc::unbounded_channel();

        let worker = PortWorker {
            devices: DeviceTree::new(),
            portsc: portsc.clone(),
            portpmsc: portpmsc.clone(),
            event_sender,
            msg_sender: msg_sender.clone(),
            msg_recv,
            async_runtime: async_runtime.clone(),
        };

        async_runtime.spawn(worker.run());

        Self {
            portsc,
            portpmsc,
            msg_sender,
        }
    }

    pub fn write_portsc(&self, port_id: usize, value: u64) -> anyhow::Result<()> {
        let was_suspended = self.portsc[port_id].is_suspended();
        self.portsc[port_id].write(value)?;

        if value & portsc::PR != 0 {
            // The reset of the port reaches a hub on it, which starts from scratch.
            self.msg_sender
                .send(PortMessage::RootPortReset(port_id as u8))
                .map_err(|_| anyhow!("port channel closed"))?;
        }
        let is_suspended = self.portsc[port_id].is_suspended();
        if is_suspended != was_suspended {
            debug!(
                "root port {port_id} is {}",
                if is_suspended {
                    "suspended"
                } else {
                    "active again"
                }
            );
            // A hub on the port learns that the bus is suspended, or active again.
            self.msg_sender
                .send(PortMessage::RootPortSuspended(port_id as u8, is_suspended))
                .map_err(|_| anyhow!("port channel closed"))?;
        }
        Ok(())
    }

    pub fn read_portsc(&self, port_id: usize) -> u64 {
        self.portsc[port_id].read()
    }

    pub fn write_portpmsc(&self, port_id: usize, value: u32) {
        self.portpmsc[port_id].write(value);
    }

    pub fn read_portpmsc(&self, port_id: usize) -> u32 {
        self.portpmsc[port_id].read()
    }

    pub fn create_hotplug_control(&self) -> HotplugControl<CRD> {
        HotplugControl {
            msg_send: self.msg_sender.clone(),
        }
    }

    pub fn create_device_retriever(&self) -> DeviceRetriever<CRD> {
        DeviceRetriever {
            msg_send: self.msg_sender.clone(),
        }
    }

    pub fn reset_sender(&self) -> PortArrayResetSender<CRD> {
        PortArrayResetSender {
            msg_send: self.msg_sender.clone(),
        }
    }

    fn port_version(port_id: u8) -> UsbVersion {
        match port_id as u64 {
            1..=NUM_USB3_PORTS => UsbVersion::USB3,
            id if id > NUM_USB3_PORTS && id <= MAX_PORTS => UsbVersion::USB2,
            id => panic!("asked for port version of non-existent port id {id}"),
        }
    }
}

#[derive(Debug)]
pub struct PortArrayResetSender<CRD: CompleteRealDevice> {
    msg_send: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> ResetSender for PortArrayResetSender<CRD> {
    fn send_reset(&self, completion_notifier: oneshot::Sender<()>) -> anyhow::Result<()> {
        self.msg_send
            .send(PortMessage::ResetPorts(completion_notifier))
            .map_err(|_| anyhow!("port channel closed"))
    }
}

#[derive(Debug)]
struct PortWorker<CRD: CompleteRealDevice> {
    devices: DeviceTree<CRD>,
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    portpmsc: Arc<OneIndexed<PortpmscRegister, { MAX_PORTS as usize }>>,
    event_sender: EventSender,
    // the worker does not use the sender itself but needs to pass clones of the sender to detach listeners
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
    msg_recv: mpsc::UnboundedReceiver<PortMessage<CRD>>,
    async_runtime: runtime::Handle,
}

/// A device in the tree, as listed by [`HotplugControl::tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode<ID> {
    pub path: PortPath,
    pub id: ID,
    pub speed: Speed,
    /// The number of downstream ports, for a hub.
    pub hub_ports: Option<u8>,
}

#[derive(Debug)]
pub enum PortMessage<CRD: CompleteRealDevice> {
    // Attaches a device at a port path, or on the first free root port that fits its speed.
    Attach(Option<PortPath>, CRD, oneshot::Sender<Response>),
    // Detaches a device, optionally limited to a specific attached device lifecycle.
    Detach(CRD::ID, Option<DeviceInstanceId>, oneshot::Sender<Response>),
    // Detaches the device at a port path with everything below it.
    DetachAt(PortPath, oneshot::Sender<Response>),
    ListAttached(oneshot::Sender<Vec<CRD::ID>>),
    ListTree(oneshot::Sender<Vec<TreeNode<CRD::ID>>>),
    // root hub port number, route string
    GetDevice(u8, u32, oneshot::Sender<Option<Arc<CRD>>>),
    // The upstream port of a hub on the root port is reset
    RootPortReset(u8),
    // The root port of a hub is suspended (true) or resumed (false)
    RootPortSuspended(u8, bool),
    // Host Controller Reset
    ResetPorts(oneshot::Sender<()>),
}

impl<CRD: CompleteRealDevice> PortWorker<CRD> {
    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => info!("PortWorker stopped {err}"),
        }
    }

    // this function should only return with an error, but we cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            match self.next_msg().await? {
                PortMessage::Attach(path, device, responder) => {
                    responder.send_anyhow(self.attach(path, device)?)?;
                }
                PortMessage::Detach(identifier, instance_id, responder) => {
                    responder.send_anyhow(self.detach(identifier, instance_id)?)?;
                }
                PortMessage::DetachAt(path, responder) => {
                    responder.send_anyhow(self.detach_at(&path)?)?;
                }
                PortMessage::ListAttached(responder) => {
                    responder.send_anyhow(self.attached_devices())?;
                }
                PortMessage::ListTree(responder) => {
                    responder.send_anyhow(self.tree())?;
                }
                PortMessage::GetDevice(root_port, route_string, responder) => {
                    let device = self
                        .devices
                        .lookup(root_port, route_string)
                        .map(|node| node.device.clone());
                    responder.send_anyhow(device)?;
                }
                PortMessage::RootPortReset(port_id) => {
                    self.devices.reset_hub_on_root_port(port_id);
                }
                PortMessage::RootPortSuspended(port_id, suspended) => {
                    self.devices.set_root_hub_suspended(port_id, suspended);
                }
                PortMessage::ResetPorts(completion) => {
                    self.reset_ports()?;
                    completion.send_anyhow(())?;
                }
            };
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<PortMessage<CRD>> {
        self.msg_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("port channel closed"))
    }

    fn attach(&mut self, path: Option<PortPath>, device: CRD) -> anyhow::Result<Response> {
        // A device that is attached already is replaced, but only by a device that is accepted.
        let existing = self.path_of(device.identifier(), None);

        let speed = match device.realdevice_ref().speed() {
            Some(speed) => speed,
            None => return Ok(Response::CouldNotDetermineSpeed),
        };
        let version = UsbVersion::from_speed(speed);

        let path = match path {
            Some(path) => path,
            None => {
                let available_port_id = match (1..=MAX_PORTS as usize).find(|&i| {
                    (self.devices.root_port_is_free(i as u8)
                        || existing == PortPath::root(i as u8).ok())
                        && self.portsc[i].usb_version() == version
                }) {
                    Some(port) => port,
                    None => return Ok(Response::NoFreePort),
                };
                PortPath::root(available_port_id as u8).expect("root port numbers start at 1")
            }
        };
        let root_port = path.root_port() as usize;
        match self.portsc.get(root_port) {
            Some(portsc) if portsc.usb_version() != version => {
                return Ok(Response::SpeedNotSupported)
            }
            Some(_) => {}
            None => return Ok(Response::NoSuchPath),
        }
        if let Err(response) = self.devices.check_free(&path, speed, existing.as_ref()) {
            return Ok(response);
        }

        if let Some(existing) = existing {
            info!(
                "A device with the same identifier is already attached and will be detached first"
            );
            self.detach_at(&existing)?;
        }

        let identifier = device.identifier();
        let node = Node::new(device);
        let hub_handle = node.hub_handle().cloned();
        let instance_id = node.instance_id.clone();
        let detach_token = node.device.detach_token();
        if let Err(response) = self.devices.attach(path, node, speed) {
            return Ok(response);
        }
        self.async_runtime.spawn(detach_listener(
            detach_token,
            identifier,
            instance_id,
            self.msg_sender.clone(),
        ));

        if path.is_root() {
            self.portsc[root_port].set(connected_portsc(version, speed));
            self.connect_hub_to_root_port(root_port, hub_handle);

            let event = EventTrb::new_port_status_change_event_trb(path.root_port());
            self.event_sender.send(event)?;
        }

        info!("Attached {speed} device {identifier:?} at port path {path}");

        Ok(Response::SuccessfulOperation)
    }

    /// A hub on a root port can be suspended by the driver, and wakes the root port when it has
    /// a reason to.
    fn connect_hub_to_root_port(&self, root_port: usize, hub: Option<HubHandle>) {
        let Some(hub) = hub else {
            return;
        };
        self.portsc[root_port].set_suspend_supported(true);
        let portsc = self.portsc.clone();
        hub.connect_to_root_port(move || match portsc[root_port].remote_wakeup() {
            Ok(true) => debug!("root port {root_port} is resumed by the hub on it"),
            Ok(false) => {}
            Err(err) => warn!("could not wake root port {root_port}: {err}"),
        });
    }

    /// Bring the port registers into the state a Host Controller Reset leaves them in.
    ///
    /// A device that is still connected shows up as freshly connected, like on a real controller
    /// that detects the device after the reset. The event for it reaches the interrupter after its
    /// reset. It cannot go onto an event ring yet, but it sets USBSTS.PCD for the driver, which
    /// also looks at the ports on its own.
    fn reset_ports(&self) -> anyhow::Result<()> {
        // The hubs lose their power, too.
        self.devices.reset_hubs();

        for port_id in 1..=MAX_PORTS as usize {
            let speed = PortPath::root(port_id as u8)
                .ok()
                .and_then(|path| self.devices.node(&path))
                .and_then(Node::speed);
            let value = speed.map_or(portsc::PP | portsc::value::PLS_RXDETECT, |speed| {
                connected_portsc(self.portsc[port_id].usb_version(), speed)
            });
            self.portsc[port_id].set(value);
            self.portpmsc[port_id].write(0);

            if speed.is_some() {
                let event = EventTrb::new_port_status_change_event_trb(port_id as u8);
                self.event_sender.send(event)?;
            }
        }

        Ok(())
    }

    fn attached_devices(&self) -> Vec<CRD::ID> {
        self.devices
            .nodes()
            .into_iter()
            .map(|(_, node)| node.device.identifier())
            .collect()
    }

    fn tree(&self) -> Vec<TreeNode<CRD::ID>> {
        self.devices
            .nodes()
            .into_iter()
            .filter_map(|(path, node)| {
                Some(TreeNode {
                    path,
                    id: node.device.identifier(),
                    speed: node.speed()?,
                    hub_ports: node.hub_handle().map(HubHandle::ports),
                })
            })
            .collect()
    }

    /// The path of the device with an identifier, optionally limited to a particular attach
    /// lifecycle.
    fn path_of(
        &self,
        id: CRD::ID,
        requested_instance_id: Option<DeviceInstanceId>,
    ) -> Option<PortPath> {
        self.devices
            .nodes()
            .into_iter()
            .find(|(_, node)| {
                let instance_id_matches = requested_instance_id
                    .as_ref()
                    .is_none_or(|requested_instance_id| requested_instance_id == &node.instance_id);
                node.device.identifier() == id && instance_id_matches
            })
            .map(|(path, _)| path)
    }

    /// Detach a device by identifier, optionally requiring a particular attach lifecycle.
    ///
    /// An absent instance ID is used by explicit detach and reattach requests, which operate on
    /// the currently attached device with instance ID. An instance ID is used by asynchronous detach
    /// listeners and must match the currently attached device before it is removed.
    fn detach(
        &mut self,
        id: CRD::ID,
        requested_instance_id: Option<DeviceInstanceId>,
    ) -> anyhow::Result<Response> {
        self.path_of(id, requested_instance_id).map_or_else(
            || {
                // This message is expected once per soft detach:
                // - this handler runs
                // - cancels the detach token
                // - detach_listener_tasks notices the cancellation and calls this handler again
                // - second handler of course now cannot find this devices
                //
                // It is also expected when a detach listener targets an instance that has been
                // replaced by reattachment.
                //
                // However, this message will also be printed when detach command for unknown identifier
                // is received.
                debug!("Could not find the device to detach");
                Ok(Response::NoSuchDevice)
            },
            |path| {
                debug!("Device to detach is connected at port path {path}");
                self.detach_at(&path)
            },
        )
    }

    /// Detach the device at a port path, with the devices below it if it is a hub.
    fn detach_at(&mut self, path: &PortPath) -> anyhow::Result<Response> {
        let Some(node) = self.devices.detach(path) else {
            debug!("There is no device at port path {path}");
            return Ok(Response::NoSuchDevice);
        };
        let identifier = node.device.identifier();

        // inform everybody else (endpoint handles) about the detach, so that they can drop
        // their reference of the device, too. This includes the devices below a hub.
        node.cancel_detach_tokens();

        if path.is_root() {
            let port_id = path.root_port();
            // update portsc register
            self.portsc[usize::from(port_id)].set_suspend_supported(false);
            self.portsc[usize::from(port_id)].set(portsc::PP | portsc::CSC);

            // send port status change event
            let event = EventTrb::new_port_status_change_event_trb(port_id);
            self.event_sender.send(event)?;
        }

        info!("Detached device {identifier:?} from port path {path}");

        Ok(Response::SuccessfulOperation)
    }
}

/// The PORTSC value of a port right after a device of the given speed connected.
const fn connected_portsc(version: UsbVersion, speed: Speed) -> u64 {
    match version {
        UsbVersion::USB3 => {
            portsc::CCS
                | portsc::PED
                | portsc::PP
                | portsc::CSC
                | portsc::PEC
                | portsc::PRC
                | (speed as u64) << 10
        }
        UsbVersion::USB2 => {
            portsc::CCS
                | portsc::value::PLS_POLLING
                | portsc::PP
                | (speed as u64) << 10
                | portsc::CSC
        }
    }
}

async fn detach_listener<CRD: CompleteRealDevice>(
    cancel: CancellationToken,
    identifier: CRD::ID,
    instance_id: DeviceInstanceId,
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
) {
    let (send, recv) = oneshot::channel();
    cancel.cancelled().await;
    let _ = msg_sender.send(PortMessage::Detach(identifier, Some(instance_id), send));
    let _ = recv.await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbVersion {
    USB2,
    USB3,
}

impl UsbVersion {
    const fn from_speed(speed: Speed) -> Self {
        if speed.is_usb2_speed() {
            Self::USB2
        } else {
            Self::USB3
        }
    }
}

// Helper function to get port id from MMIO address
const fn get_port_id_from_addr(
    addr: u64,
    base_addr: u64,
    port_count: u64,
    register_offset: u64,
) -> Option<usize> {
    if addr >= base_addr && addr < base_addr + (port_count * offset::PORT_STRIDE) {
        // Check if this is the correct register within the port's PORT_STRIDE byte range
        if (addr - base_addr) % offset::PORT_STRIDE == register_offset {
            Some(((addr - base_addr) / offset::PORT_STRIDE) as usize + 1)
        } else {
            None
        }
    } else {
        None
    }
}

pub const fn get_portsc_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0)
}

pub const fn get_portpmsc_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x4)
}

pub const fn get_portli_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x8)
}

#[derive(Debug, Clone)]
pub struct HotplugControl<CRD: CompleteRealDevice> {
    msg_send: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> HotplugControl<CRD> {
    /// Attach a device on the first free root port that fits its speed.
    pub async fn attach(&self, device: CRD) -> Response {
        self.attach_at_path(None, device).await
    }

    /// Attach a device at a port path. The hubs on the way must exist.
    pub async fn attach_at(&self, path: PortPath, device: CRD) -> Response {
        self.attach_at_path(Some(path), device).await
    }

    async fn attach_at_path(&self, path: Option<PortPath>, device: CRD) -> Response {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::Attach(path, device, responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    /// Detach the device at a port path, with the devices below it if it is a hub.
    pub async fn detach_at(&self, path: PortPath) -> Response {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::DetachAt(path, responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    /// Detach every device, a hub with the devices below it. Returns how many devices were on the
    /// root ports.
    pub async fn detach_all(&self) -> usize {
        let roots: Vec<PortPath> = self
            .tree()
            .await
            .into_iter()
            .map(|node| node.path)
            .filter(PortPath::is_root)
            .collect();
        for path in &roots {
            self.detach_at(*path).await;
        }
        roots.len()
    }

    /// All devices with their port paths, every hub before the devices below it.
    pub async fn tree(&self) -> Vec<TreeNode<CRD::ID>> {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::ListTree(responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    pub async fn detach(&self, identifier: CRD::ID) -> Response {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::Detach(identifier, None, responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    pub async fn list_devices(&self) -> Vec<CRD::ID> {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::ListAttached(responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }
}

#[derive(Debug, Clone)]
pub struct DeviceRetriever<CRD: CompleteRealDevice> {
    msg_send: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> DeviceRetriever<CRD> {
    /// The device that a slot context describes with its root hub port number and route string.
    pub async fn get_device(
        &self,
        root_port: u8,
        route_string: u32,
    ) -> anyhow::Result<Option<Arc<CRD>>> {
        let (send, recv) = oneshot::channel();
        self.msg_send
            .send(PortMessage::GetDevice(root_port, route_string, send))?;
        let device = recv.await?;

        Ok(device)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{runtime::Handle, time::timeout};

    use crate::device::xhci::{
        interrupter::tests::testutils::MockInterrupter,
        real_device::{tests::testutils::MockRealDevice, CompleteRealDeviceImpl},
    };

    use super::*;

    const ASYNC_TIMEOUT_SECS: u64 = 30;
    const PORT_ID: u8 = 1;
    const USB_BUS_NR: u8 = 1;
    const USB_DEV_NR: u8 = 1;
    const IDENTIFIER: (u8, u8) = (USB_BUS_NR, USB_DEV_NR);

    #[tokio::test]
    async fn port_array_hotplug_control_can_attach_list_and_detach() {
        let async_runtime = Handle::current();
        let (event_sender, mut interrupter) = MockInterrupter::new();

        let mock_real_device = CompleteRealDeviceImpl::new(IDENTIFIER, MockRealDevice::default());
        let port_array: PortArray<CompleteRealDeviceImpl<MockRealDevice, (u8, u8)>> =
            PortArray::new(event_sender, async_runtime);

        // attach a device
        let hotplug_control = port_array.create_hotplug_control();
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.attach(mock_real_device),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, Response::SuccessfulOperation);
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_port_status_change_event_trb(PORT_ID))
        );

        // list attached devices
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.list_devices(),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, vec![IDENTIFIER]);
        assert!(interrupter.is_empty());

        // detach the device
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.detach(IDENTIFIER),
        )
        .await
        .expect("local timeout on await");

        // expect a successful response for the command and an event
        assert_eq!(response, Response::SuccessfulOperation);
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_port_status_change_event_trb(PORT_ID))
        );

        // list attached devices
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.list_devices(),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, vec![]);
        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn host_controller_reset_restores_the_ports_but_keeps_the_devices_connected() {
        let async_runtime = Handle::current();
        let (event_sender, mut interrupter) = MockInterrupter::new();
        let port_array: PortArray<CompleteRealDeviceImpl<MockRealDevice, (u8, u8)>> =
            PortArray::new(event_sender, async_runtime);
        let hotplug_control = port_array.create_hotplug_control();

        let response = hotplug_control
            .attach(CompleteRealDeviceImpl::new(
                IDENTIFIER,
                MockRealDevice::default(),
            ))
            .await;
        assert_eq!(response, Response::SuccessfulOperation);
        assert!(interrupter.await_event().await.is_some());
        let connected = port_array.read_portsc(PORT_ID as usize);
        assert_eq!(connected & portsc::CCS, portsc::CCS);

        // The driver enumerated the device and cleared the change bits, and then the guest restarts.
        port_array
            .write_portsc(PORT_ID as usize, portsc::CSC | portsc::PEC | portsc::PRC)
            .unwrap();
        assert_eq!(port_array.read_portsc(PORT_ID as usize) & portsc::CSC, 0);
        port_array.write_portpmsc(PORT_ID as usize, 0x1234);
        port_array.write_portsc(2, portsc::PR).unwrap();
        assert!(interrupter.await_event().await.is_some());

        timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            port_array.reset_sender().reset().unwrap(),
        )
        .await
        .expect("local timeout on await")
        .unwrap();

        // The connected device is found as new.
        assert_eq!(port_array.read_portsc(PORT_ID as usize), connected);
        assert_eq!(port_array.read_portpmsc(PORT_ID as usize), 0);
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_port_status_change_event_trb(PORT_ID)),
            "the interrupter learns about the connected device"
        );
        // An empty port went back to its initial state.
        assert_eq!(
            port_array.read_portsc(2),
            portsc::PP | portsc::value::PLS_RXDETECT
        );
        assert!(interrupter.is_empty());
        assert_eq!(
            timeout(
                Duration::from_secs(ASYNC_TIMEOUT_SECS),
                hotplug_control.list_devices()
            )
            .await
            .expect("local timeout on await"),
            vec![IDENTIFIER]
        );
    }

    mod tree {
        use super::*;
        use crate::device::xhci::{
            hub_ports::{port_feature, HubShared},
            real_device::tests::testutils::MockRealDeviceWithSpeed,
        };

        type TreeDevice = CompleteRealDeviceImpl<MockRealDeviceWithSpeed, (u8, u8)>;
        type TreePortArray = PortArray<TreeDevice>;

        fn path(text: &str) -> PortPath {
            text.parse().unwrap()
        }

        fn device(id: u8, speed: Speed) -> TreeDevice {
            CompleteRealDeviceImpl::new((id, id), MockRealDeviceWithSpeed(speed))
        }

        fn hub(id: u8, ports: u8, speed: Speed) -> (TreeDevice, Arc<HubShared>) {
            let shared = Arc::new(HubShared::new(ports, speed));
            (
                device(id, speed).with_hub(HubHandle::new(shared.clone())),
                shared,
            )
        }

        fn port_array() -> (
            TreePortArray,
            crate::device::xhci::interrupter::tests::testutils::MockInterrupter,
        ) {
            let (event_sender, interrupter) = MockInterrupter::new();
            (PortArray::new(event_sender, Handle::current()), interrupter)
        }

        async fn eventually<T>(mut f: impl AsyncFnMut() -> Option<T>) -> T {
            for _ in 0..300 {
                if let Some(value) = f().await {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the condition did not become true");
        }

        #[tokio::test]
        async fn a_hub_on_a_root_port_holds_devices_that_are_found_by_route_string() {
            let (port_array, mut interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let retriever = port_array.create_device_retriever();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            let hub_token = hub_device.detach_token();
            let child = device(2, Speed::Full);
            let child_token = child.detach_token();

            assert_eq!(
                control.attach_at(path("5"), hub_device).await,
                Response::SuccessfulOperation
            );
            assert_eq!(
                interrupter.await_event().await,
                Some(EventTrb::new_port_status_change_event_trb(5))
            );
            assert_eq!(
                control.attach_at(path("5.2"), child).await,
                Response::SuccessfulOperation
            );
            // The hub tells its driver, not the root port.
            assert!(interrupter.is_empty());
            assert_eq!(hub_shared.port_status(2).unwrap()[0] & 1, 0, "not powered");
            hub_shared.set_port_feature(2, port_feature::POWER).unwrap();
            assert_eq!(hub_shared.status_change_bitmap(), Some(vec![1 << 2]));

            let tree = control.tree().await;
            assert_eq!(
                tree,
                [
                    TreeNode {
                        path: path("5"),
                        id: (1, 1),
                        speed: Speed::High,
                        hub_ports: Some(4)
                    },
                    TreeNode {
                        path: path("5.2"),
                        id: (2, 2),
                        speed: Speed::Full,
                        hub_ports: None
                    },
                ]
            );
            let found = async |root, route| {
                retriever
                    .get_device(root, route)
                    .await
                    .unwrap()
                    .map(|device| device.identifier())
            };
            assert_eq!(found(5, 0).await, Some((1, 1)));
            assert_eq!(found(5, 0x2).await, Some((2, 2)));
            assert_eq!(found(5, 0x3).await, None);
            assert_eq!(found(5, 0x22).await, None);
            assert_eq!(found(6, 0).await, None);

            // Detaching the hub takes the device below with it.
            assert_eq!(
                control.detach_at(path("5")).await,
                Response::SuccessfulOperation
            );
            assert_eq!(
                interrupter.await_event().await,
                Some(EventTrb::new_port_status_change_event_trb(5))
            );
            assert!(hub_token.is_cancelled());
            assert!(child_token.is_cancelled());
            assert_eq!(control.tree().await, []);
            assert_eq!(control.list_devices().await, []);
            assert_eq!(found(5, 0x2).await, None);
            assert_eq!(control.detach_at(path("5")).await, Response::NoSuchDevice);
        }

        #[tokio::test]
        async fn attaching_at_a_path_checks_the_port_and_the_speed() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (full_hub, _) = hub(1, 2, Speed::Full);
            assert_eq!(
                control.attach_at(path("6"), full_hub).await,
                Response::SuccessfulOperation
            );

            let checks = [
                ("6.1", device(2, Speed::High), Response::SpeedNotSupported),
                ("6.3", device(3, Speed::Full), Response::NoSuchPath),
                ("6.1.1", device(4, Speed::Full), Response::NoSuchPath),
                ("7.1", device(5, Speed::Full), Response::NoSuchPath),
                ("9", device(6, Speed::Full), Response::NoSuchPath),
                ("6", device(7, Speed::Full), Response::PathOccupied),
                // A super speed device does not go to a USB 2 port, and a USB 2 device does not
                // go to a USB 3 port.
                ("5", device(8, Speed::Super), Response::SpeedNotSupported),
                ("1", device(9, Speed::Full), Response::SpeedNotSupported),
            ];
            for (at, device, expected) in checks {
                assert_eq!(control.attach_at(path(at), device).await, expected, "{at}");
            }
            assert_eq!(
                control.attach_at(path("6.2"), device(10, Speed::Low)).await,
                Response::SuccessfulOperation
            );
            assert_eq!(
                control.attach_at(path("1"), device(11, Speed::Super)).await,
                Response::SuccessfulOperation
            );
            // Only what was accepted is in the tree.
            let paths: Vec<_> = control
                .tree()
                .await
                .into_iter()
                .map(|node| node.path.to_string())
                .collect();
            assert_eq!(paths, ["1", "6", "6.2"]);
        }

        /// The paths of the devices in the tree, with their identifiers.
        async fn placed(control: &HotplugControl<TreeDevice>) -> Vec<(String, (u8, u8))> {
            control
                .tree()
                .await
                .into_iter()
                .map(|node| (node.path.to_string(), node.id))
                .collect()
        }

        #[tokio::test]
        async fn an_attach_that_is_refused_leaves_the_device_with_the_same_identifier_alone() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (full_hub, _) = hub(1, 4, Speed::Full);
            assert_eq!(
                control.attach_at(path("5"), full_hub).await,
                Response::SuccessfulOperation
            );
            let first = device(2, Speed::Full);
            let first_token = first.detach_token();
            assert_eq!(
                control.attach_at(path("5.1"), first).await,
                Response::SuccessfulOperation
            );
            assert_eq!(
                control.attach_at(path("5.2"), device(3, Speed::Full)).await,
                Response::SuccessfulOperation
            );
            let before = placed(&control).await;

            // The same device again, to places that do not take it.
            for (at, device, expected) in [
                ("5.2", device(2, Speed::Full), Response::PathOccupied),
                ("5.9", device(2, Speed::Full), Response::NoSuchPath),
                ("9", device(2, Speed::Full), Response::NoSuchPath),
                ("5.3", device(2, Speed::High), Response::SpeedNotSupported),
                ("1", device(2, Speed::Full), Response::SpeedNotSupported),
                ("5.1.1", device(2, Speed::Full), Response::NotAHub),
            ] {
                assert_eq!(control.attach_at(path(at), device).await, expected, "{at}");
                assert_eq!(placed(&control).await, before, "{at}");
                assert!(!first_token.is_cancelled(), "{at}");
            }
        }

        #[tokio::test]
        async fn an_attach_of_a_device_that_is_attached_already_moves_it() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (full_hub, _) = hub(1, 4, Speed::Full);
            assert_eq!(
                control.attach_at(path("5"), full_hub).await,
                Response::SuccessfulOperation
            );
            let first = device(2, Speed::Full);
            let first_token = first.detach_token();
            assert_eq!(
                control.attach_at(path("5.1"), first).await,
                Response::SuccessfulOperation
            );

            // To another port, and then to the port it is on, with a new instance each time.
            let second = device(2, Speed::Full);
            let second_token = second.detach_token();
            assert_eq!(
                control.attach_at(path("5.3"), second).await,
                Response::SuccessfulOperation
            );
            assert!(first_token.is_cancelled());
            assert_eq!(
                placed(&control).await,
                [("5".to_string(), (1, 1)), ("5.3".to_string(), (2, 2))]
            );
            assert_eq!(
                control.attach_at(path("5.3"), device(2, Speed::Full)).await,
                Response::SuccessfulOperation
            );
            assert!(second_token.is_cancelled());
            assert_eq!(placed(&control).await.len(), 2);
        }

        #[tokio::test]
        async fn a_hub_replaced_by_a_device_takes_the_ports_below_it_away() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_a, _) = hub(1, 4, Speed::High);
            let (hub_b, _) = hub(2, 4, Speed::High);
            for (at, device) in [("5", hub_a), ("5.1", hub_b)] {
                assert_eq!(
                    control.attach_at(path(at), device).await,
                    Response::SuccessfulOperation
                );
            }
            let before = placed(&control).await;

            // A device with the identifier of the hub on the way cannot go below that hub, which
            // is gone when the device is attached.
            assert_eq!(
                control
                    .attach_at(path("5.1.2"), device(2, Speed::Full))
                    .await,
                Response::NoSuchPath
            );
            assert_eq!(placed(&control).await, before);
            // It can take the place of the hub.
            assert_eq!(
                control.attach_at(path("5.1"), device(2, Speed::Full)).await,
                Response::SuccessfulOperation
            );
        }

        #[tokio::test]
        async fn a_device_without_a_path_that_is_attached_already_takes_a_free_root_port() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            assert_eq!(
                control.attach(device(1, Speed::Full)).await,
                Response::SuccessfulOperation
            );
            assert_eq!(placed(&control).await, [("5".to_string(), (1, 1))]);

            // Its own port is free when it is replaced.
            assert_eq!(
                control.attach(device(1, Speed::Full)).await,
                Response::SuccessfulOperation
            );
            assert_eq!(placed(&control).await, [("5".to_string(), (1, 1))]);

            // A device that finds no port keeps the one it has.
            for id in 2..=4 {
                assert_eq!(
                    control.attach(device(id, Speed::Full)).await,
                    Response::SuccessfulOperation
                );
            }
            assert_eq!(
                control.attach(device(5, Speed::Full)).await,
                Response::NoFreePort
            );
            assert_eq!(placed(&control).await.len(), 4);
        }

        #[tokio::test]
        async fn a_device_without_a_path_goes_to_the_first_free_root_port_of_its_version() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();

            for (id, speed, expected) in [
                (1, Speed::Super, "1"),
                (2, Speed::High, "5"),
                (3, Speed::Full, "6"),
                (4, Speed::Super, "2"),
            ] {
                assert_eq!(
                    control.attach(device(id, speed)).await,
                    Response::SuccessfulOperation
                );
                let tree = control.tree().await;
                assert_eq!(
                    tree.iter()
                        .find(|node| node.id == (id, id))
                        .unwrap()
                        .path
                        .to_string(),
                    expected
                );
            }
        }

        #[tokio::test]
        async fn a_device_below_a_hub_is_detached_by_its_identifier_or_by_its_token() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            control.attach_at(path("5"), hub_device).await;
            control.attach_at(path("5.1"), device(2, Speed::Full)).await;
            let third = device(3, Speed::Full);
            let third_token = third.detach_token();
            control.attach_at(path("5.2"), third).await;
            for port in 1..=2 {
                hub_shared
                    .set_port_feature(port, port_feature::POWER)
                    .unwrap();
            }

            assert_eq!(control.detach((2, 2)).await, Response::SuccessfulOperation);
            assert_eq!(control.detach((2, 2)).await, Response::NoSuchDevice);
            // The hub reports the removal to its driver.
            assert_eq!(hub_shared.port_status(1).unwrap()[2] & 1, 1);

            // A device that goes away by itself, like one that was unplugged on the host.
            third_token.cancel();
            eventually(async || (control.tree().await.len() == 1).then_some(())).await;
            assert_eq!(hub_shared.port_status(2).unwrap()[2] & 1, 1);
        }

        #[tokio::test]
        async fn detaching_all_takes_every_hub_and_device_off_and_cancels_their_tokens() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, _) = hub(1, 4, Speed::High);
            let child = device(2, Speed::Full);
            let other = device(3, Speed::High);
            let tokens = [&hub_device, &child, &other].map(|d| d.detach_token());
            for (at, device) in [("5", hub_device), ("5.1", child), ("6", other)] {
                assert_eq!(
                    control.attach_at(path(at), device).await,
                    Response::SuccessfulOperation
                );
            }

            assert_eq!(control.detach_all().await, 2);
            assert!(control.tree().await.is_empty());
            assert!(tokens.iter().all(CancellationToken::is_cancelled));
            assert_eq!(control.detach_all().await, 0);
        }

        #[tokio::test]
        async fn a_device_that_is_attached_again_replaces_the_old_one_with_the_same_identifier() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            control.attach_at(path("5"), hub(1, 4, Speed::High).0).await;
            control.attach_at(path("5.1"), device(2, Speed::Full)).await;

            assert_eq!(
                control.attach_at(path("5.3"), device(2, Speed::Full)).await,
                Response::SuccessfulOperation
            );

            let paths: Vec<_> = control
                .tree()
                .await
                .into_iter()
                .map(|node| node.path.to_string())
                .collect();
            assert_eq!(paths, ["5", "5.3"]);
        }

        #[tokio::test]
        async fn a_reset_of_the_root_port_resets_the_hub_on_it() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            let (other_device, other_shared) = hub(2, 4, Speed::High);
            control.attach_at(path("5"), hub_device).await;
            control.attach_at(path("6"), other_device).await;
            hub_shared.set_configuration(1);
            other_shared.set_configuration(1);

            port_array.write_portsc(5, portsc::PR).unwrap();
            // The worker handles the messages in order, so the answer to this one comes after.
            control.list_devices().await;

            assert_eq!(hub_shared.configuration(), 0);
            assert_eq!(other_shared.configuration(), 1);
        }

        #[tokio::test]
        async fn a_hub_on_a_suspended_root_port_wakes_it_when_a_device_comes() {
            let (port_array, mut interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            control.attach_at(path("5"), hub_device).await;
            interrupter.await_event().await;
            // The driver enables the port with a reset, powers the hub port, arms the wakeup of
            // the hub and suspends the port.
            port_array.write_portsc(5, portsc::PR).unwrap();
            interrupter.await_event().await;
            // The worker handles the reset of the hub before it answers this.
            control.list_devices().await;
            hub_shared.set_port_feature(1, port_feature::POWER).unwrap();
            hub_shared.set_remote_wakeup(true);
            port_array
                .write_portsc(5, portsc::LWS | portsc::value::PLS_U3)
                .unwrap();
            assert_eq!(
                port_array.read_portsc(5) & portsc::PLS,
                portsc::value::PLS_U3
            );
            // The suspend reaches the worker before the attach does.
            assert_eq!(control.list_devices().await.len(), 1);
            assert!(interrupter.is_empty());

            control.attach_at(path("5.1"), device(2, Speed::Full)).await;

            assert_eq!(
                port_array.read_portsc(5) & portsc::PLS,
                portsc::value::PLS_RESUME
            );
            assert_eq!(
                interrupter.await_event().await,
                Some(EventTrb::new_port_status_change_event_trb(5))
            );
        }

        #[tokio::test]
        async fn a_hub_on_an_awake_root_port_does_not_wake_it() {
            let (port_array, mut interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            control.attach_at(path("5"), hub_device).await;
            interrupter.await_event().await;
            hub_shared.set_port_feature(1, port_feature::POWER).unwrap();
            hub_shared.set_remote_wakeup(true);

            control.attach_at(path("5.1"), device(2, Speed::Full)).await;

            assert_eq!(
                port_array.read_portsc(5) & portsc::PLS,
                portsc::value::PLS_POLLING
            );
            assert!(interrupter.is_empty());
        }

        #[tokio::test]
        async fn a_device_on_a_root_port_cannot_be_suspended() {
            let (port_array, mut interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            control.attach_at(path("6"), device(1, Speed::Full)).await;
            interrupter.await_event().await;
            port_array.write_portsc(6, portsc::PR).unwrap();
            interrupter.await_event().await;

            port_array
                .write_portsc(6, portsc::LWS | portsc::value::PLS_U3)
                .unwrap();

            assert_eq!(
                port_array.read_portsc(6) & portsc::PLS,
                portsc::value::PLS_U0
            );
        }

        #[tokio::test]
        async fn a_host_controller_reset_resets_all_hubs_but_keeps_the_devices() {
            let (port_array, _interrupter) = port_array();
            let control = port_array.create_hotplug_control();
            let (hub_device, hub_shared) = hub(1, 4, Speed::High);
            control.attach_at(path("5"), hub_device).await;
            control.attach_at(path("5.1"), device(2, Speed::Full)).await;
            hub_shared.set_configuration(1);
            hub_shared.set_port_feature(1, port_feature::POWER).unwrap();

            timeout(
                Duration::from_secs(ASYNC_TIMEOUT_SECS),
                port_array.reset_sender().reset().unwrap(),
            )
            .await
            .expect("local timeout on await")
            .unwrap();

            assert_eq!(hub_shared.configuration(), 0);
            assert_eq!(hub_shared.port_status(1).unwrap(), [0, 0, 0, 0]);
            assert_eq!(control.tree().await.len(), 2);
            assert_eq!(
                port_array.read_portsc(5) & portsc::CCS,
                portsc::CCS,
                "the hub is connected to its root port"
            );
        }
    }
}
