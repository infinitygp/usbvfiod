//! The reply to the framed commands.

use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

use serde::{Deserialize, Serialize};

use super::{response::Response, spec::TreeEntry};

/// The longest reply that we accept, to not allocate what a broken peer announces.
const MAX_REPLY_SIZE: u32 = 1 << 20;

/// The reply to a framed command: the length as 32-bit little endian number, then that many bytes
/// of JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub response: Response,
    /// What went wrong, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The devices, for the reply to the tree command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tree: Vec<TreeEntry>,
}

impl Reply {
    #[must_use]
    pub const fn new(response: Response) -> Self {
        Self {
            response,
            message: None,
            tree: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_message(response: Response, message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..Self::new(response)
        }
    }

    pub fn send_over_socket(&self, socket: &mut UnixStream) -> io::Result<()> {
        let json = serde_json::to_vec(self)?;
        let length = u32::try_from(json.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the reply is too long"))?;
        let mut message = length.to_le_bytes().to_vec();
        message.extend(json);
        socket.write_all(&message)
    }

    pub fn receive_from_socket(socket: &mut UnixStream) -> io::Result<Self> {
        let mut length = [0u8; 4];
        socket.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length);
        if length > MAX_REPLY_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the reply announces {length} bytes"),
            ));
        }
        let mut json = vec![0u8; length as usize];
        socket.read_exact(&mut json)?;
        Ok(serde_json::from_slice(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotplug_protocol::{port_path::PortPath, spec::UsbSpeed};

    #[test]
    fn a_reply_with_a_tree_crosses_a_socket() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let reply = Reply {
            response: Response::SuccessfulOperation,
            message: None,
            tree: vec![TreeEntry {
                path: "5.1".parse::<PortPath>().unwrap(),
                speed: UsbSpeed::Full,
                id: "virtual 3".to_string(),
                hub_ports: None,
            }],
        };

        reply.send_over_socket(&mut server).unwrap();

        assert_eq!(Reply::receive_from_socket(&mut client).unwrap(), reply);
    }

    #[test]
    fn an_error_reply_carries_its_message() {
        let (mut client, mut server) = UnixStream::pair().unwrap();

        Reply::with_message(Response::PathOccupied, "5.1 is taken")
            .send_over_socket(&mut server)
            .unwrap();

        let reply = Reply::receive_from_socket(&mut client).unwrap();
        assert_eq!(reply.response, Response::PathOccupied);
        assert_eq!(reply.message.as_deref(), Some("5.1 is taken"));
        assert!(reply.tree.is_empty());
    }

    #[test]
    fn an_announced_length_beyond_the_limit_is_refused() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(&u32::MAX.to_le_bytes()).unwrap();

        Reply::receive_from_socket(&mut client).unwrap_err();
    }
}
