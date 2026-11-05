//! The position of a device in the USB topology.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How many hubs a path can go through below the root port: a route string has five tiers.
pub const MAX_HUB_TIERS: usize = 5;

/// The highest downstream port number of a hub, which is what a nibble of a route string holds.
pub const MAX_HUB_PORT: u8 = 15;

/// The port path of a device: `R` for a device on the root port `R` of the controller, and
/// `R.p1.p2` for a device on port `p2` of the hub on port `p1` of the hub on root port `R`.
///
/// All numbers start at 1. `R` is the number of the root port as it is used in the PORTSC
/// registers of the controller, and `p` is the number of the downstream port of a hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PortPath {
    root: u8,
    /// The hub ports, in order from the root port. The unused entries are zero.
    ports: [u8; MAX_HUB_TIERS],
    depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PortPathError {
    #[error("the port path is empty")]
    Empty,
    #[error("{0:?} is not a port number")]
    NotANumber(String),
    #[error("the root port number starts at 1")]
    RootPortZero,
    #[error("hub port {0} is out of range, a hub has the ports 1 to {MAX_HUB_PORT}")]
    HubPortOutOfRange(u8),
    #[error("the path goes through more than {MAX_HUB_TIERS} hubs")]
    TooDeep,
}

impl PortPath {
    /// The path of the device on a root port.
    pub const fn root(root: u8) -> Result<Self, PortPathError> {
        if root == 0 {
            return Err(PortPathError::RootPortZero);
        }
        Ok(Self {
            root,
            ports: [0; MAX_HUB_TIERS],
            depth: 0,
        })
    }

    /// The path of a device on the root port `root`, behind the hub ports `hub_ports`.
    pub fn new(root: u8, hub_ports: &[u8]) -> Result<Self, PortPathError> {
        let mut path = Self::root(root)?;
        for &port in hub_ports {
            path = path.child(port)?;
        }
        Ok(path)
    }

    /// The path of the device on a port of the hub at this path.
    pub const fn child(&self, port: u8) -> Result<Self, PortPathError> {
        if port == 0 || port > MAX_HUB_PORT {
            return Err(PortPathError::HubPortOutOfRange(port));
        }
        if self.depth as usize == MAX_HUB_TIERS {
            return Err(PortPathError::TooDeep);
        }
        let mut child = *self;
        child.ports[self.depth as usize] = port;
        child.depth += 1;
        Ok(child)
    }

    /// The root port number.
    #[must_use]
    pub const fn root_port(&self) -> u8 {
        self.root
    }

    /// The port numbers of the hubs on the way, starting at the hub on the root port.
    #[must_use]
    pub fn hub_ports(&self) -> &[u8] {
        &self.ports[..self.depth as usize]
    }

    /// Whether the device is on a root port.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.depth == 0
    }

    /// The path of the hub that holds this device, or none for a device on a root port.
    #[must_use]
    pub const fn parent(&self) -> Option<Self> {
        if self.depth == 0 {
            return None;
        }
        let mut parent = *self;
        parent.depth -= 1;
        parent.ports[parent.depth as usize] = 0;
        Some(parent)
    }

    /// The port of the parent hub that the device is on, or none for a device on a root port.
    #[must_use]
    pub fn parent_port(&self) -> Option<u8> {
        self.hub_ports().last().copied()
    }

    /// The route string of the xHCI slot context: 4 bits per tier, with the port of the first hub
    /// in the lowest nibble. A device on a root port has the route string 0.
    #[must_use]
    pub fn route_string(&self) -> u32 {
        self.hub_ports()
            .iter()
            .enumerate()
            .fold(0, |route, (tier, &port)| {
                route | u32::from(port) << (4 * tier)
            })
    }

    /// The path of the device that a slot context describes with its root hub port number and
    /// route string. A route string with a gap or bits above the fifth tier names no device.
    #[must_use]
    pub fn from_route(root: u8, route: u32) -> Option<Self> {
        let mut path = Self::root(root).ok()?;
        let mut remaining = route;
        while remaining != 0 {
            // A gap is a zero nibble, which is no hub port, and a sixth tier is too deep.
            path = path.child((remaining & 0xf) as u8).ok()?;
            remaining >>= 4;
        }
        Some(path)
    }
}

impl fmt::Display for PortPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.root)?;
        for port in self.hub_ports() {
            write!(f, ".{port}")?;
        }
        Ok(())
    }
}

impl FromStr for PortPath {
    type Err = PortPathError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(PortPathError::Empty);
        }
        fn number(part: &str) -> Result<u8, PortPathError> {
            part.parse()
                .map_err(|_| PortPathError::NotANumber(part.to_string()))
        }

        let (root, hub_ports) = text.split_once('.').unwrap_or((text, ""));
        let mut path = Self::root(number(root)?)?;
        if !hub_ports.is_empty() || text.ends_with('.') {
            for part in hub_ports.split('.') {
                path = path.child(number(part)?)?;
            }
        }
        Ok(path)
    }
}

impl From<PortPath> for String {
    fn from(path: PortPath) -> Self {
        path.to_string()
    }
}

impl TryFrom<String> for PortPath {
    type Error = PortPathError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_through_text() {
        for text in ["1", "5", "8.1", "5.1.1.4", "5.15.15.15.15.15"] {
            let path: PortPath = text.parse().unwrap();
            assert_eq!(path.to_string(), text);
        }
        let path: PortPath = "5.1.2.3".parse().unwrap();
        assert_eq!(path.root_port(), 5);
        assert_eq!(path.hub_ports(), [1, 2, 3]);
        assert!(!path.is_root());
        assert!("7".parse::<PortPath>().unwrap().is_root());
    }

    #[test]
    fn malformed_paths_are_refused() {
        assert_eq!("".parse::<PortPath>(), Err(PortPathError::Empty));
        assert_eq!("0".parse::<PortPath>(), Err(PortPathError::RootPortZero));
        assert_eq!(
            "5.0".parse::<PortPath>(),
            Err(PortPathError::HubPortOutOfRange(0))
        );
        assert_eq!(
            "5.16".parse::<PortPath>(),
            Err(PortPathError::HubPortOutOfRange(16))
        );
        assert_eq!(
            "5.1.2.3.4.5.6".parse::<PortPath>(),
            Err(PortPathError::TooDeep)
        );
        for text in ["5.", ".5", "5..1", "a", "5.x", "256", "-1", "5 .1"] {
            assert!(text.parse::<PortPath>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn route_string_has_the_first_tier_in_the_lowest_nibble() {
        assert_eq!(PortPath::new(5, &[]).unwrap().route_string(), 0);
        assert_eq!(PortPath::new(5, &[1]).unwrap().route_string(), 0x1);
        assert_eq!(PortPath::new(5, &[1, 4]).unwrap().route_string(), 0x41);
        assert_eq!(
            PortPath::new(6, &[15, 2, 3, 4, 5]).unwrap().route_string(),
            0x54_32f
        );
    }

    #[test]
    fn route_strings_map_back_to_paths() {
        for text in ["5", "5.1", "6.1.4", "8.15.2.3.4.5"] {
            let path: PortPath = text.parse().unwrap();
            assert_eq!(
                PortPath::from_route(path.root_port(), path.route_string()),
                Some(path)
            );
        }
        // A gap names no device, and neither do bits above the fifth tier or root port 0.
        assert_eq!(PortPath::from_route(5, 0x100), None);
        assert_eq!(PortPath::from_route(5, 0x101), None);
        assert_eq!(PortPath::from_route(5, 0x10_0000), None);
        assert_eq!(PortPath::from_route(0, 0), None);
    }

    #[test]
    fn parent_and_child_are_inverse() {
        let path: PortPath = "5.1.4".parse().unwrap();

        assert_eq!(path.parent_port(), Some(4));
        assert_eq!(path.parent(), Some("5.1".parse().unwrap()));
        assert_eq!(path.parent().unwrap().child(4), Ok(path));
        assert_eq!("5".parse::<PortPath>().unwrap().parent(), None);
        assert_eq!("5".parse::<PortPath>().unwrap().parent_port(), None);
        assert_eq!(
            PortPath::new(5, &[1, 1, 1, 1, 1]).unwrap().child(1),
            Err(PortPathError::TooDeep)
        );
    }

    #[test]
    fn paths_are_json_strings() {
        let path: PortPath = "5.1.1.4".parse().unwrap();

        let json = serde_json::to_string(&path).unwrap();

        assert_eq!(json, "\"5.1.1.4\"");
        assert_eq!(serde_json::from_str::<PortPath>(&json).unwrap(), path);
        serde_json::from_str::<PortPath>("\"5.0\"").unwrap_err();
    }
}
