use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use vmm_sys_util::errno::Error;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use super::{port_path::PortPath, spec::HubSpec, spec::UsbSpeed};

// The first three commands are three bytes long: the command and two numbers, with a file
// descriptor for the attach command.
const COMMAND_ATTACH: u8 = 0;
const COMMAND_DETACH: u8 = 1;
const COMMAND_LIST: u8 = 2;

// The framed commands have an ID of their own range: the command, the length of the payload as
// 32-bit little endian number and a JSON payload of that length. The attach command takes a file
// descriptor as well.
const COMMAND_ATTACH_AT: u8 = 0x10;
const COMMAND_ATTACH_HUB: u8 = 0x11;
const COMMAND_ATTACH_VIRTUAL_HID: u8 = 0x12;
const COMMAND_DETACH_AT: u8 = 0x13;
const COMMAND_TREE: u8 = 0x14;

/// The longest payload that we accept, to not allocate what a broken peer announces.
const MAX_PAYLOAD_SIZE: u32 = 1 << 16;

#[derive(Debug)]
pub enum Command {
    /// Attach a device of the host on the first free root port that fits its speed.
    Attach { bus: u8, device: u8, fd: File },
    /// Detach a device of the host by its bus and device number.
    Detach { bus: u8, device: u8 },
    /// List the devices of the host.
    List,
    /// Attach a device of the host at a port path. The hubs on the way must exist.
    AttachAt {
        path: PortPath,
        bus: u8,
        device: u8,
        fd: File,
    },
    /// Attach a virtual hub at a port path.
    AttachHub { path: PortPath, spec: HubSpec },
    /// Attach the virtual test HID device at a port path.
    AttachVirtualHid { path: PortPath, speed: UsbSpeed },
    /// Detach the device at a port path, with everything below it.
    DetachAt { path: PortPath },
    /// List the whole device tree.
    Tree,
}

#[derive(Serialize, Deserialize)]
struct AttachAtPayload {
    path: PortPath,
    bus: u8,
    device: u8,
}

#[derive(Serialize, Deserialize)]
struct AttachHubPayload {
    path: PortPath,
    spec: HubSpec,
}

#[derive(Serialize, Deserialize)]
struct AttachVirtualHidPayload {
    path: PortPath,
    speed: UsbSpeed,
}

#[derive(Serialize, Deserialize)]
struct DetachAtPayload {
    path: PortPath,
}

fn framed<T: Serialize>(id: u8, payload: &T) -> Result<Vec<u8>, CommandSendError> {
    let json = serde_json::to_vec(payload)?;
    let length = u32::try_from(json.len())
        .ok()
        .filter(|length| *length <= MAX_PAYLOAD_SIZE)
        .ok_or(CommandSendError::PayloadTooLarge(json.len()))?;
    let mut buf = vec![id];
    buf.extend(length.to_le_bytes());
    buf.extend(json);
    Ok(buf)
}

fn parse_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CommandReceiveError> {
    Ok(serde_json::from_slice(payload)?)
}

impl Command {
    pub fn send_over_socket(self, socket: &UnixStream) -> Result<(), CommandSendError> {
        let id = self.variant_to_id();
        let (buf, fd) = match &self {
            Self::Attach { bus, device, fd } => (vec![id, *bus, *device], Some(fd.as_raw_fd())),
            Self::Detach { bus, device } => (vec![id, *bus, *device], None),
            Self::List => (vec![id, 0, 0], None),
            Self::AttachAt {
                path,
                bus,
                device,
                fd,
            } => (
                framed(
                    id,
                    &AttachAtPayload {
                        path: *path,
                        bus: *bus,
                        device: *device,
                    },
                )?,
                Some(fd.as_raw_fd()),
            ),
            Self::AttachHub { path, spec } => (
                framed(
                    id,
                    &AttachHubPayload {
                        path: *path,
                        spec: spec.clone(),
                    },
                )?,
                None,
            ),
            Self::AttachVirtualHid { path, speed } => (
                framed(
                    id,
                    &AttachVirtualHidPayload {
                        path: *path,
                        speed: *speed,
                    },
                )?,
                None,
            ),
            Self::DetachAt { path } => (framed(id, &DetachAtPayload { path: *path })?, None),
            Self::Tree => (framed(id, &())?, None),
        };

        let transmitted = fd.map_or_else(
            || socket.send_with_fds(&[&buf[..]], &[]),
            |fd| socket.send_with_fd(&buf[..], fd),
        )?;

        // TODO implement a transmission loop to be safe (we should not run
        // into problems with how little data we send, though).
        if transmitted == buf.len() {
            Ok(())
        } else {
            Err(CommandSendError::NotSentEnough(buf.len(), transmitted))
        }
    }

    pub fn receive_from_socket(socket: &UnixStream) -> Result<Self, CommandReceiveError> {
        // The file descriptor arrives with the first byte of the message, so read the command alone
        // first and decide from it how much follows.
        let mut id = [0u8; 1];
        let (bytes_read, file) = socket.recv_with_fd(&mut id[..])?;
        if bytes_read != id.len() {
            return Err(CommandReceiveError::NotEnoughData(id.len(), bytes_read));
        }
        let mut reader = socket;

        match (id[0], file) {
            (COMMAND_ATTACH | COMMAND_DETACH | COMMAND_LIST, file) => {
                let mut numbers = [0u8; 2];
                reader.read_exact(&mut numbers)?;
                match (id[0], file) {
                    (COMMAND_ATTACH, Some(file)) => Ok(Self::Attach {
                        bus: numbers[0],
                        device: numbers[1],
                        fd: file,
                    }),
                    (COMMAND_ATTACH, None) => Err(CommandReceiveError::MissingFd),
                    (COMMAND_DETACH, None) => Ok(Self::Detach {
                        bus: numbers[0],
                        device: numbers[1],
                    }),
                    (_, None) => Ok(Self::List {}),
                    (_, Some(_)) => Err(CommandReceiveError::UnexpectedFd),
                }
            }
            (
                id @ (COMMAND_ATTACH_AT
                | COMMAND_ATTACH_HUB
                | COMMAND_ATTACH_VIRTUAL_HID
                | COMMAND_DETACH_AT
                | COMMAND_TREE),
                file,
            ) => {
                let payload = read_payload(&mut reader)?;
                match (id, file) {
                    (COMMAND_ATTACH_AT, Some(fd)) => {
                        let AttachAtPayload { path, bus, device } = parse_payload(&payload)?;
                        Ok(Self::AttachAt {
                            path,
                            bus,
                            device,
                            fd,
                        })
                    }
                    (COMMAND_ATTACH_AT, None) => Err(CommandReceiveError::MissingFd),
                    (_, Some(_)) => Err(CommandReceiveError::UnexpectedFd),
                    (COMMAND_ATTACH_HUB, None) => {
                        let AttachHubPayload { path, spec } = parse_payload(&payload)?;
                        Ok(Self::AttachHub { path, spec })
                    }
                    (COMMAND_ATTACH_VIRTUAL_HID, None) => {
                        let AttachVirtualHidPayload { path, speed } = parse_payload(&payload)?;
                        Ok(Self::AttachVirtualHid { path, speed })
                    }
                    (COMMAND_DETACH_AT, None) => {
                        let DetachAtPayload { path } = parse_payload(&payload)?;
                        Ok(Self::DetachAt { path })
                    }
                    (_, None) => Ok(Self::Tree),
                }
            }
            (command, None) => Err(CommandReceiveError::UnknownCommand(command)),
            (_, Some(_)) => Err(CommandReceiveError::UnexpectedFd),
        }
    }

    const fn variant_to_id(&self) -> u8 {
        match self {
            Self::Attach {
                bus: _,
                device: _,
                fd: _,
            } => COMMAND_ATTACH,
            Self::Detach { bus: _, device: _ } => COMMAND_DETACH,
            Self::List => COMMAND_LIST,
            Self::AttachAt { .. } => COMMAND_ATTACH_AT,
            Self::AttachHub { .. } => COMMAND_ATTACH_HUB,
            Self::AttachVirtualHid { .. } => COMMAND_ATTACH_VIRTUAL_HID,
            Self::DetachAt { .. } => COMMAND_DETACH_AT,
            Self::Tree => COMMAND_TREE,
        }
    }
}

/// Read the length and the payload of a framed command.
fn read_payload(reader: &mut &UnixStream) -> Result<Vec<u8>, CommandReceiveError> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length);
    if length > MAX_PAYLOAD_SIZE {
        return Err(CommandReceiveError::PayloadTooLarge(length));
    }
    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

#[derive(thiserror::Error, Debug)]
pub enum CommandReceiveError {
    #[error("did not receive enough data over the socket. Expected {0}, received {1}")]
    NotEnoughData(usize, usize),
    #[error("expected to receive a file descriptor, but there was none")]
    MissingFd,
    #[error("did not expect to receive a file descriptor, but there was one")]
    UnexpectedFd,
    #[error("Unknown command")]
    UnknownCommand(u8),
    #[error("the command announces a payload of {0} bytes")]
    PayloadTooLarge(u32),
    #[error("the payload of the command is malformed: {0}")]
    MalformedPayload(#[from] serde_json::Error),
    #[error("could not read the rest of the command: {0}")]
    Io(#[from] io::Error),
    #[error("Encountered errno during socket IO")]
    ErrnoError(#[from] Error),
}

#[derive(thiserror::Error, Debug)]
pub enum CommandSendError {
    #[error("did not receive enough data over the socket. Expected to send {0}, sent {1}")]
    NotSentEnough(usize, usize),
    #[error("the payload of {0} bytes is too large")]
    PayloadTooLarge(usize),
    #[error("could not encode the payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Encountered errno during socket IO")]
    ErrnoError(#[from] Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::hotplug_protocol::spec::UsbSpeed;

    fn path(text: &str) -> PortPath {
        text.parse().unwrap()
    }

    fn transmit(command: Command) -> Result<Command, CommandReceiveError> {
        let (client, server) = UnixStream::pair().unwrap();
        command.send_over_socket(&client).unwrap();
        Command::receive_from_socket(&server)
    }

    fn a_file() -> File {
        File::open("/dev/null").unwrap()
    }

    #[test]
    fn the_three_byte_commands_still_work() {
        match transmit(Command::Attach {
            bus: 9,
            device: 3,
            fd: a_file(),
        })
        .unwrap()
        {
            Command::Attach { bus, device, .. } => assert_eq!((bus, device), (9, 3)),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            transmit(Command::Detach { bus: 9, device: 3 }).unwrap(),
            Command::Detach { bus: 9, device: 3 }
        ));
        assert!(matches!(transmit(Command::List).unwrap(), Command::List));
    }

    #[test]
    fn a_legacy_client_sends_the_bytes_the_way_it_always_did() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[1, 9, 3]).unwrap();

        assert!(matches!(
            Command::receive_from_socket(&server).unwrap(),
            Command::Detach { bus: 9, device: 3 }
        ));
    }

    #[test]
    fn attaching_a_host_device_at_a_path_carries_the_file_descriptor() {
        match transmit(Command::AttachAt {
            path: path("5.1.1.4"),
            bus: 5,
            device: 17,
            fd: a_file(),
        })
        .unwrap()
        {
            Command::AttachAt {
                path: received,
                bus,
                device,
                fd,
            } => {
                assert_eq!(received, path("5.1.1.4"));
                assert_eq!((bus, device), (5, 17));
                fd.metadata().unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_hub_with_its_description_crosses_the_socket() {
        let spec = HubSpec {
            vid: Some(0x067b),
            pid: Some(0x2586),
            bcd_device: Some(0),
            manufacturer: Some("USB Device".to_string()),
            product: Some("USB 2.0 Hub".to_string()),
            serial: None,
            ports: 4,
            speed: UsbSpeed::High,
            descriptors: Some("12 01 00 02".to_string()),
        };

        match transmit(Command::AttachHub {
            path: path("5"),
            spec: spec.clone(),
        })
        .unwrap()
        {
            Command::AttachHub {
                path: received,
                spec: received_spec,
            } => {
                assert_eq!(received, path("5"));
                assert_eq!(received_spec, spec);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_remaining_commands_cross_the_socket() {
        assert!(matches!(
            transmit(Command::AttachVirtualHid {
                path: path("5.2.2"),
                speed: UsbSpeed::High
            })
            .unwrap(),
            Command::AttachVirtualHid { path: p, speed: UsbSpeed::High } if p == path("5.2.2")
        ));
        assert!(matches!(
            transmit(Command::DetachAt { path: path("5.1") }).unwrap(),
            Command::DetachAt { path: p } if p == path("5.1")
        ));
        assert!(matches!(transmit(Command::Tree).unwrap(), Command::Tree));
    }

    #[test]
    fn the_file_descriptor_of_an_attach_command_is_required_and_others_refuse_it() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[0, 9, 3]).unwrap();
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::MissingFd)
        ));

        let (client, server) = UnixStream::pair().unwrap();
        let payload = framed(COMMAND_TREE, &()).unwrap();
        client
            .send_with_fd(&payload[..], a_file().as_raw_fd())
            .unwrap();
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::UnexpectedFd)
        ));
    }

    #[test]
    fn malformed_frames_are_refused() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[0x77, 0, 0]).unwrap();
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::UnknownCommand(0x77))
        ));

        // An announced payload beyond the limit is not read.
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut frame = vec![COMMAND_DETACH_AT];
        frame.extend((MAX_PAYLOAD_SIZE + 1).to_le_bytes());
        client.write_all(&frame).unwrap();
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::PayloadTooLarge(_))
        ));

        // The payload is not JSON of the right shape.
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut frame = vec![COMMAND_DETACH_AT];
        frame.extend(2u32.to_le_bytes());
        frame.extend(b"{}");
        client.write_all(&frame).unwrap();
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::MalformedPayload(_))
        ));

        // The peer hangs up in the middle of the payload.
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut frame = vec![COMMAND_DETACH_AT];
        frame.extend(30u32.to_le_bytes());
        frame.extend(b"{\"path\"");
        client.write_all(&frame).unwrap();
        drop(client);
        assert!(matches!(
            Command::receive_from_socket(&server),
            Err(CommandReceiveError::Io(_))
        ));
    }
}
