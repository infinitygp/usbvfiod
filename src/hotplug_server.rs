//! Receive runtime commands

use std::{
    fs::File,
    os::unix::net::{UnixListener, UnixStream},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use tokio::runtime;
use tracing::{debug, warn};
use usbvfiod::hotplug_protocol::{
    command::Command,
    port_path::PortPath,
    reply::Reply,
    response::Response,
    spec::{HubSpec, TreeEntry, UsbSpeed},
};

use crate::device::xhci::{
    any_device::{AnyDevice, ControllerDevice},
    nusb::NusbRealDevice,
    port::HotplugControl,
    real_device::{CompleteRealDeviceImpl, DeviceId, Speed},
    virtual_hid::VirtualHid,
    virtual_hub::VirtualHub,
};

/// How long we wait for the rest of a command from a client that started to send one. The server
/// handles one client at a time.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The identifier of the next emulated device.
static NEXT_VIRTUAL_ID: AtomicU32 = AtomicU32::new(1);

pub fn run_hotplug_server(
    socket: UnixListener,
    hotplug_control: HotplugControl<ControllerDevice>,
    async_runtime: runtime::Handle,
) {
    loop {
        if let Ok((mut stream, _addr)) = socket.accept() {
            if let Err(e) = stream.set_read_timeout(Some(COMMAND_TIMEOUT)) {
                warn!("Could not set a timeout for reading a hotplug command: {e}");
            }
            match Command::receive_from_socket(&stream) {
                Ok(command) => {
                    debug!("Received command {:?} on hotplug socket", command);
                    if let Err(e) =
                        handle_command(command, &mut stream, &hotplug_control, &async_runtime)
                    {
                        // The error contains all the necessary context
                        warn!("{:?}", e);
                    }
                }
                Err(e) => warn!("Error occurred while reading a hotplug command {}", e),
            }
        }
    }
}

fn handle_command(
    command: Command,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<ControllerDevice>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    match command {
        Command::Attach {
            bus,
            device: dev,
            fd,
        } => handle_attach(bus, dev, fd, socket, hotplug_control, async_runtime)
            .context("Failed to handle attach command")?,
        Command::Detach { bus, device } => {
            handle_detach(bus, device, socket, hotplug_control, async_runtime)
                .context("Failed to handle detach command")?;
        }
        Command::List => {
            let devices = async_runtime
                .block_on(hotplug_control.list_devices())
                .into_iter()
                .filter_map(|id| match id {
                    DeviceId::Host { bus, device } => Some((bus, device)),
                    DeviceId::Virtual(_) => None,
                })
                .collect();
            Response::ListFollowing
                .send_device_list(devices, socket)
                .context("Failed to handle list command")?;
        }
        Command::AttachAt {
            path,
            bus,
            device,
            fd,
        } => {
            let reply = match attach_host_device(fd, bus, device, async_runtime) {
                Ok(device) => {
                    let response = async_runtime.block_on(hotplug_control.attach_at(path, device));
                    reply_to_attach(response, path)
                }
                Err(err) => Reply::with_message(Response::FailedToOpenFd, format!("{err:#}")),
            };
            reply
                .send_over_socket(socket)
                .context("Failed to send the reply to the attach-at command")?;
        }
        Command::AttachHub { path, spec } => {
            let reply = match virtual_hub(&spec) {
                Ok(device) => {
                    let response = async_runtime.block_on(hotplug_control.attach_at(path, device));
                    reply_to_attach(response, path)
                }
                Err(err) => Reply::with_message(Response::InvalidRequest, format!("{err:#}")),
            };
            reply
                .send_over_socket(socket)
                .context("Failed to send the reply to the attach-hub command")?;
        }
        Command::AttachVirtualHid { path, speed } => {
            let reply = match virtual_hid(speed) {
                Ok(device) => {
                    let response = async_runtime.block_on(hotplug_control.attach_at(path, device));
                    reply_to_attach(response, path)
                }
                Err(err) => Reply::with_message(Response::InvalidRequest, format!("{err:#}")),
            };
            reply
                .send_over_socket(socket)
                .context("Failed to send the reply to the attach-virtual-hid command")?;
        }
        Command::DetachAt { path } => {
            let response = async_runtime.block_on(hotplug_control.detach_at(path));
            let reply = match response {
                Response::NoSuchDevice => {
                    Reply::with_message(response, format!("there is no device at {path}"))
                }
                response => Reply::new(response),
            };
            reply
                .send_over_socket(socket)
                .context("Failed to send the reply to the detach-at command")?;
        }
        Command::Tree => {
            let tree = async_runtime
                .block_on(hotplug_control.tree())
                .into_iter()
                .map(|node| TreeEntry {
                    path: node.path,
                    speed: node.speed.into(),
                    id: node.id.to_string(),
                    hub_ports: node.hub_ports,
                })
                .collect();
            Reply {
                tree,
                ..Reply::new(Response::SuccessfulOperation)
            }
            .send_over_socket(socket)
            .context("Failed to send the reply to the tree command")?;
        }
    }

    Ok(())
}

fn next_virtual_id() -> DeviceId {
    DeviceId::Virtual(NEXT_VIRTUAL_ID.fetch_add(1, Ordering::Relaxed))
}

/// Open a device of the host from the file descriptor that the client sent.
fn attach_host_device(
    fd: File,
    bus: u8,
    dev: u8,
    async_runtime: &runtime::Handle,
) -> Result<ControllerDevice> {
    let device = nusb::Device::from_fd(fd.into())
        .wait()
        .context("Failed to open nusb device from the supplied file descriptor")?;
    let real_device = NusbRealDevice::try_new(device, async_runtime.clone())?;
    Ok(CompleteRealDeviceImpl::new(
        DeviceId::Host { bus, device: dev },
        AnyDevice::new(real_device),
    ))
}

fn virtual_hub(spec: &HubSpec) -> Result<ControllerDevice> {
    let (hub, handle) = VirtualHub::new(spec)?;
    Ok(CompleteRealDeviceImpl::new(next_virtual_id(), AnyDevice::new(hub)).with_hub(handle))
}

fn virtual_hid(speed: UsbSpeed) -> Result<ControllerDevice> {
    let speed = match speed {
        UsbSpeed::Full => Speed::Full,
        UsbSpeed::High => Speed::High,
        other => anyhow::bail!(
            "the virtual HID device is a full speed or a high speed device, not a {other} one"
        ),
    };
    Ok(CompleteRealDeviceImpl::new(
        next_virtual_id(),
        AnyDevice::new(VirtualHid::new(speed)),
    ))
}

/// The reply to a command that attaches a device at a path, with the reason of a failure.
fn reply_to_attach(response: Response, path: PortPath) -> Reply {
    let message = match response {
        Response::PathOccupied => format!("a device is attached at {path} already"),
        Response::NoSuchPath => format!(
            "{path} does not exist: the root port, the hub on the way or its port is missing"
        ),
        Response::NotAHub => format!("the device on the way to {path} is not a hub"),
        Response::SpeedNotSupported => {
            format!("the port at {path} does not take a device of this speed")
        }
        Response::CouldNotDetermineSpeed => "the speed of the device is unknown".to_string(),
        _ => return Reply::new(response),
    };
    Reply::with_message(response, message)
}

fn handle_attach(
    bus: u8,
    dev: u8,
    fd: File,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<ControllerDevice>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    let complete_device = attach_host_device(fd, bus, dev, async_runtime)?;
    let response = async_runtime.block_on(hotplug_control.attach(complete_device));
    response
        .send_over_socket(socket)
        .context("Successfully performed hot-plug command, but failed to send the response")?;

    Ok(())
}

fn handle_detach(
    bus: u8,
    dev: u8,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<ControllerDevice>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    let response =
        async_runtime.block_on(hotplug_control.detach(DeviceId::Host { bus, device: dev }));
    response
        .send_over_socket(socket)
        .context("Successfully performed detach command, but failed to send the response")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, thread};

    use super::*;
    use crate::xhci_backend::XhciBackend;

    /// A hotplug server on a socket in front of a controller.
    struct TestServer {
        path: PathBuf,
    }

    impl TestServer {
        fn start(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("usbvfiod-hotplug-{}-{name}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let socket = UnixListener::bind(&path).unwrap();
            let handle = runtime::Handle::current();
            let backend = XhciBackend::<ControllerDevice>::new(handle.clone()).unwrap();
            let control = backend.hotplug_control();
            // The server runs for the rest of the process, like the controller behind it.
            thread::spawn(move || {
                let _backend = backend;
                run_hotplug_server(socket, control, handle);
            });
            Self { path }
        }

        fn connect(&self) -> UnixStream {
            UnixStream::connect(&self.path).unwrap()
        }

        fn ask(&self, command: Command) -> Reply {
            let mut socket = self.connect();
            command.send_over_socket(&socket).unwrap();
            Reply::receive_from_socket(&mut socket).unwrap()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn path(text: &str) -> PortPath {
        text.parse().unwrap()
    }

    fn hub_spec(ports: u8, speed: UsbSpeed) -> HubSpec {
        HubSpec {
            vid: Some(0x067b),
            pid: Some(0x2586),
            bcd_device: Some(0),
            manufacturer: Some("USB Device".to_string()),
            product: Some("USB 2.0 Hub".to_string()),
            serial: None,
            ports,
            speed,
            descriptors: None,
        }
    }

    fn tree_paths(server: &TestServer) -> Vec<String> {
        let reply = server.ask(Command::Tree);
        assert_eq!(reply.response, Response::SuccessfulOperation);
        reply
            .tree
            .into_iter()
            .map(|entry| entry.path.to_string())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hubs_and_devices_are_attached_listed_and_detached_at_port_paths() {
        let server = TestServer::start("tree");

        let hub = server.ask(Command::AttachHub {
            path: path("5"),
            spec: hub_spec(4, UsbSpeed::High),
        });
        assert_eq!(hub.response, Response::SuccessfulOperation, "{hub:?}");
        for (at, speed) in [("5.1", UsbSpeed::Full), ("5.2", UsbSpeed::High)] {
            let reply = server.ask(Command::AttachVirtualHid {
                path: path(at),
                speed,
            });
            assert_eq!(
                reply.response,
                Response::SuccessfulOperation,
                "{at}: {reply:?}"
            );
        }

        let tree = server.ask(Command::Tree).tree;
        assert_eq!(
            tree.iter()
                .map(|entry| (entry.path.to_string(), entry.speed, entry.hub_ports))
                .collect::<Vec<_>>(),
            [
                ("5".to_string(), UsbSpeed::High, Some(4)),
                ("5.1".to_string(), UsbSpeed::Full, None),
                ("5.2".to_string(), UsbSpeed::High, None),
            ]
        );
        assert!(tree.iter().all(|entry| entry.id.starts_with("virtual ")));

        // The legacy list only knows the devices of the host.
        let mut socket = server.connect();
        Command::List.send_over_socket(&socket).unwrap();
        let response = Response::receive_from_socket(&mut socket).unwrap();
        assert_eq!(response, Response::ListFollowing);
        assert_eq!(response.receive_devices_list(&mut socket).unwrap(), []);

        // Detaching the hub takes its devices with it, and the path can be used again.
        assert_eq!(
            server.ask(Command::DetachAt { path: path("5") }).response,
            Response::SuccessfulOperation
        );
        assert_eq!(tree_paths(&server), Vec::<String>::new());
        let again = server.ask(Command::AttachVirtualHid {
            path: path("5"),
            speed: UsbSpeed::Full,
        });
        assert_eq!(again.response, Response::SuccessfulOperation);
        assert_eq!(tree_paths(&server), ["5"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refused_commands_say_why() {
        let server = TestServer::start("refused");
        let attach_hub = |at: &str, ports, speed| {
            server.ask(Command::AttachHub {
                path: path(at),
                spec: hub_spec(ports, speed),
            })
        };
        let attach_hid = |at: &str, speed| {
            server.ask(Command::AttachVirtualHid {
                path: path(at),
                speed,
            })
        };

        assert_eq!(
            attach_hub("5", 4, UsbSpeed::Full).response,
            Response::SuccessfulOperation
        );

        // A hub with 16 ports or a super speed hub does not exist.
        for reply in [
            attach_hub("6", 16, UsbSpeed::High),
            attach_hub("6", 0, UsbSpeed::High),
            attach_hub("6", 4, UsbSpeed::Super),
            attach_hid("6", UsbSpeed::Low),
        ] {
            assert_eq!(reply.response, Response::InvalidRequest, "{reply:?}");
            assert!(reply.message.is_some());
        }
        let mut bad_descriptors = hub_spec(4, UsbSpeed::High);
        bad_descriptors.descriptors = Some("12 01".to_string());
        let reply = server.ask(Command::AttachHub {
            path: path("6"),
            spec: bad_descriptors,
        });
        assert_eq!(reply.response, Response::InvalidRequest);

        // A high speed device below a full speed hub.
        let reply = attach_hid("5.1", UsbSpeed::High);
        assert_eq!(reply.response, Response::SpeedNotSupported);
        assert!(reply.message.unwrap().contains("5.1"));
        assert_eq!(
            attach_hid("5.1", UsbSpeed::Full).response,
            Response::SuccessfulOperation
        );
        assert_eq!(
            attach_hid("5.1", UsbSpeed::Full).response,
            Response::PathOccupied
        );
        assert_eq!(
            attach_hid("5.5", UsbSpeed::Full).response,
            Response::NoSuchPath
        );
        assert_eq!(
            attach_hid("6.1", UsbSpeed::Full).response,
            Response::NoSuchPath
        );
        assert_eq!(
            attach_hid("5.1.1", UsbSpeed::Full).response,
            Response::NotAHub
        );
        assert_eq!(
            server.ask(Command::DetachAt { path: path("6") }).response,
            Response::NoSuchDevice
        );
        assert_eq!(tree_paths(&server), ["5", "5.1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hub_with_captured_descriptors_is_accepted() {
        let server = TestServer::start("captured");
        let mut spec = hub_spec(4, UsbSpeed::High);
        spec.descriptors = Some(
            "12 01 00 02 09 00 01 40 7b 06 86 25 00 00 01 03 00 01 \
             09 02 19 00 01 01 00 e0 32 09 04 00 00 01 09 00 00 00 07 05 81 03 01 00 0c"
                .to_string(),
        );

        let reply = server.ask(Command::AttachHub {
            path: path("8"),
            spec,
        });

        assert_eq!(reply.response, Response::SuccessfulOperation, "{reply:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_hangs_up_does_not_stop_the_server() {
        let server = TestServer::start("hangup");

        drop(server.connect());
        {
            use std::io::Write;
            // A command that is cut short.
            server.connect().write_all(&[0x13, 5]).unwrap();
        }

        assert_eq!(tree_paths(&server), Vec::<String>::new());
    }
}
