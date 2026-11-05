//! The devices below the root ports of the controller.
//!
//! A root port holds a device, and a hub holds a device for each of its downstream ports. A
//! device is addressed by its [`PortPath`], or by the root hub port number and route string of its
//! slot context.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use usbvfiod::hotplug_protocol::{port_path::PortPath, response::Response};

use crate::{
    device::{
        pci::constants::xhci::MAX_PORTS,
        xhci::{
            hub_ports::HubHandle,
            real_device::{CompleteRealDevice, RealDevice, Speed},
        },
    },
    one_indexed_array::OneIndexed,
};

/// The detach token is created once per device instance and cloned by its users, so its identity
/// distinguishes attached device lifecycles. Token cancellation does not change this identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInstanceId(CancellationToken);

/// The devices below the downstream ports of a hub.
#[derive(Debug)]
struct HubPorts<CRD: CompleteRealDevice> {
    handle: HubHandle,
    /// The device on each port, the first entry for port 1.
    children: Vec<Option<Node<CRD>>>,
}

/// An attached device with everything below it.
#[derive(Debug)]
pub struct Node<CRD: CompleteRealDevice> {
    pub device: Arc<CRD>,
    /// The lifecycle ID of this attached device instance.
    pub instance_id: DeviceInstanceId,
    /// The ports of the device, if it is a hub.
    hub: Option<HubPorts<CRD>>,
}

impl<CRD: CompleteRealDevice> Node<CRD> {
    pub fn new(device: CRD) -> Self {
        let instance_id = DeviceInstanceId(device.detach_token());
        let hub = device.hub().map(|handle| HubPorts {
            children: (0..handle.ports()).map(|_| None).collect(),
            handle,
        });
        Self {
            device: Arc::new(device),
            instance_id,
            hub,
        }
    }

    pub fn hub_handle(&self) -> Option<&HubHandle> {
        self.hub.as_ref().map(|hub| &hub.handle)
    }

    /// The speed of the device, if it is known.
    pub fn speed(&self) -> Option<Speed> {
        self.device.realdevice_ref().speed()
    }

    /// Inform everybody who holds a reference to the device (the endpoint handles) and to the
    /// devices below it that they are detached, so that they drop their reference.
    pub fn cancel_detach_tokens(&self) {
        self.device.detach_token().cancel();
        for child in self.children() {
            child.cancel_detach_tokens();
        }
    }

    fn children(&self) -> impl Iterator<Item = &Self> {
        self.hub
            .iter()
            .flat_map(|hub| hub.children.iter().flatten())
    }

    fn visit<'a>(&'a self, path: PortPath, nodes: &mut Vec<(PortPath, &'a Self)>) {
        nodes.push((path, self));
        if let Some(hub) = &self.hub {
            for (index, child) in hub.children.iter().enumerate() {
                let port = u8::try_from(index + 1).expect("a hub has at most 15 ports");
                // A hub on the last tier has no room for devices below it.
                if let (Some(child), Ok(path)) = (child, path.child(port)) {
                    child.visit(path, nodes);
                }
            }
        }
    }
}

/// The devices below the root ports.
#[derive(Debug)]
pub struct DeviceTree<CRD: CompleteRealDevice> {
    roots: OneIndexed<Option<Node<CRD>>, { MAX_PORTS as usize }>,
}

impl<CRD: CompleteRealDevice> Default for DeviceTree<CRD> {
    fn default() -> Self {
        Self::new()
    }
}

impl<CRD: CompleteRealDevice> DeviceTree<CRD> {
    pub fn new() -> Self {
        Self {
            roots: [const { None }; MAX_PORTS as usize].into(),
        }
    }

    fn slot(&self, path: &PortPath) -> Option<&Option<Node<CRD>>> {
        let mut slot = self.roots.get(usize::from(path.root_port()))?;
        for &port in path.hub_ports() {
            let hub = slot.as_ref()?.hub.as_ref()?;
            slot = hub.children.get(usize::from(port) - 1)?;
        }
        Some(slot)
    }

    fn slot_mut(&mut self, path: &PortPath) -> Option<&mut Option<Node<CRD>>> {
        let mut slot = self.roots.get_mut(usize::from(path.root_port()))?;
        for &port in path.hub_ports() {
            let hub = slot.as_mut()?.hub.as_mut()?;
            slot = hub.children.get_mut(usize::from(port) - 1)?;
        }
        Some(slot)
    }

    /// The device at a port path.
    pub fn node(&self, path: &PortPath) -> Option<&Node<CRD>> {
        self.slot(path)?.as_ref()
    }

    /// The device that a slot context describes: the one on a root port, behind the hubs on the
    /// ports of the route string.
    pub fn lookup(&self, root_port: u8, route_string: u32) -> Option<&Node<CRD>> {
        self.node(&PortPath::from_route(root_port, route_string)?)
    }

    /// Whether the root port exists and has no device.
    pub fn root_port_is_free(&self, root_port: u8) -> bool {
        self.roots
            .get(usize::from(root_port))
            .is_some_and(Option::is_none)
    }

    /// All devices with their paths, every hub before the devices below it.
    pub fn nodes(&self) -> Vec<(PortPath, &Node<CRD>)> {
        let mut nodes = Vec::new();
        for (index, root) in self.roots.enumerate() {
            let root_port = u8::try_from(index).expect("there are less than 256 root ports");
            if let (Some(node), Ok(path)) = (root, PortPath::root(root_port)) {
                node.visit(path, &mut nodes);
            }
        }
        nodes
    }

    /// The error with which a device of this speed cannot be attached at the path, or none if it
    /// can. The speed of the root port is not checked here.
    ///
    /// `replacing` is the path of a device that is detached before the new one is attached: its
    /// path is free, and the hubs below it are gone.
    pub fn check_free(
        &self,
        path: &PortPath,
        speed: Speed,
        replacing: Option<&PortPath>,
    ) -> Result<(), Response> {
        let mut slot = self
            .roots
            .get(usize::from(path.root_port()))
            .ok_or(Response::NoSuchPath)?;
        let mut hub_path = PortPath::root(path.root_port()).map_err(|_| Response::NoSuchPath)?;
        let mut parent_hub = None;
        for &port in path.hub_ports() {
            let hub = slot
                .as_ref()
                .ok_or(Response::NoSuchPath)?
                .hub
                .as_ref()
                .ok_or(Response::NotAHub)?;
            if replacing == Some(&hub_path) {
                return Err(Response::NoSuchPath);
            }
            slot = hub
                .children
                .get(usize::from(port) - 1)
                .ok_or(Response::NoSuchPath)?;
            parent_hub = Some(&hub.handle);
            hub_path = hub_path.child(port).map_err(|_| Response::NoSuchPath)?;
        }
        if slot.is_some() && replacing != Some(path) {
            return Err(Response::PathOccupied);
        }
        if let Some(hub) = parent_hub {
            // The hubs are USB 2 hubs, and a full speed hub has no transaction translator that
            // lets a high speed device talk through it.
            if !speed.is_usb2_speed() || (speed == Speed::High && hub.speed() != Speed::High) {
                return Err(Response::SpeedNotSupported);
            }
        }
        Ok(())
    }

    /// Attach a device at a path. The hub on the way tells its driver about the new device.
    pub fn attach(
        &mut self,
        path: PortPath,
        node: Node<CRD>,
        speed: Speed,
    ) -> Result<(), Response> {
        self.check_free(&path, speed, None)?;
        let hub_below = node.hub_handle().cloned();
        let slot = self.slot_mut(&path).ok_or(Response::NoSuchPath)?;
        *slot = Some(node);

        if let (Some(parent), Some(port)) = (path.parent(), path.parent_port()) {
            if let Some(hub) = self.node(&parent).and_then(Node::hub_handle) {
                hub.connect(port, speed, hub_below);
            }
        }
        Ok(())
    }

    /// Remove the device at a path, with everything below it. The hub on the way tells its driver
    /// that the device is gone.
    pub fn detach(&mut self, path: &PortPath) -> Option<Node<CRD>> {
        let node = self.slot_mut(path)?.take()?;

        if let (Some(parent), Some(port)) = (path.parent(), path.parent_port()) {
            if let Some(hub) = self.node(&parent).and_then(Node::hub_handle) {
                hub.disconnect(port);
            }
        }
        Some(node)
    }

    /// A reset of the controller resets every hub, whose ports lose their power.
    pub fn reset_hubs(&self) {
        for node in self.roots.iter().flatten() {
            if let Some(hub) = node.hub_handle() {
                hub.bus_reset();
            }
        }
    }

    /// The root port of a hub was suspended or resumed.
    pub fn set_root_hub_suspended(&self, root_port: u8, suspended: bool) {
        if let Some(hub) = self
            .roots
            .get(usize::from(root_port))
            .and_then(Option::as_ref)
            .and_then(Node::hub_handle)
        {
            hub.set_suspended(suspended);
        }
    }

    /// The upstream port of a hub on a root port was reset.
    pub fn reset_hub_on_root_port(&self, root_port: u8) {
        if let Some(hub) = self
            .roots
            .get(usize::from(root_port))
            .and_then(Option::as_ref)
            .and_then(Node::hub_handle)
        {
            hub.bus_reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::xhci::{
        hub_ports::{port_change, port_feature, port_status, HubShared},
        real_device::{
            tests::testutils::MockRealDeviceWithSpeed, CompleteRealDeviceImpl, DeviceId,
        },
    };

    type Device = CompleteRealDeviceImpl<MockRealDeviceWithSpeed, DeviceId>;

    fn path(text: &str) -> PortPath {
        text.parse().unwrap()
    }

    fn device(id: u32, speed: Speed) -> Device {
        CompleteRealDeviceImpl::new(DeviceId::Virtual(id), MockRealDeviceWithSpeed(speed))
    }

    /// A hub with a view on its ports for the test.
    fn hub(id: u32, ports: u8, speed: Speed) -> (Device, Arc<HubShared>) {
        let shared = Arc::new(HubShared::new(ports, speed));
        let device = device(id, speed).with_hub(HubHandle::new(shared.clone()));
        (device, shared)
    }

    fn attach(tree: &mut DeviceTree<Device>, at: &str, device: Device) -> Result<(), Response> {
        let speed = device.realdevice_ref().speed().unwrap();
        tree.attach(path(at), Node::new(device), speed)
    }

    fn ids(tree: &DeviceTree<Device>) -> Vec<(String, DeviceId)> {
        tree.nodes()
            .into_iter()
            .map(|(path, node)| (path.to_string(), node.device.identifier()))
            .collect()
    }

    #[test]
    fn devices_are_found_by_root_port_and_route_string() {
        let mut tree = DeviceTree::new();
        attach(&mut tree, "5", hub(1, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.1", hub(2, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.1.4", device(3, Speed::Full)).unwrap();
        attach(&mut tree, "5.2", device(4, Speed::High)).unwrap();
        attach(&mut tree, "6", device(5, Speed::Low)).unwrap();

        let lookup = |root, route| {
            tree.lookup(root, route)
                .map(|node| node.device.identifier())
        };

        assert_eq!(lookup(5, 0), Some(DeviceId::Virtual(1)));
        assert_eq!(lookup(5, 0x1), Some(DeviceId::Virtual(2)));
        assert_eq!(lookup(5, 0x41), Some(DeviceId::Virtual(3)));
        assert_eq!(lookup(5, 0x2), Some(DeviceId::Virtual(4)));
        assert_eq!(lookup(6, 0), Some(DeviceId::Virtual(5)));
        // Nothing is there, or the route string makes no sense.
        assert_eq!(lookup(5, 0x3), None);
        assert_eq!(lookup(5, 0x42), None);
        assert_eq!(lookup(5, 0x241), None);
        assert_eq!(lookup(5, 0x100), None);
        assert_eq!(lookup(7, 0), None);
        assert_eq!(
            lookup(6, 0x1),
            None,
            "a device below a device that is no hub"
        );
        assert_eq!(lookup(0, 0), None);
        assert_eq!(lookup(200, 0), None);
    }

    #[test]
    fn the_listing_has_every_hub_before_the_devices_below_it() {
        let mut tree = DeviceTree::new();
        attach(&mut tree, "7", device(9, Speed::Full)).unwrap();
        attach(&mut tree, "5", hub(1, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.2", device(4, Speed::High)).unwrap();
        attach(&mut tree, "5.1", hub(2, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.1.4", device(3, Speed::Full)).unwrap();

        let listed: Vec<_> = ids(&tree).into_iter().map(|(path, _)| path).collect();

        assert_eq!(listed, ["5", "5.1", "5.1.4", "5.2", "7"]);
    }

    #[test]
    fn attaching_checks_the_path() {
        let mut tree = DeviceTree::new();
        attach(&mut tree, "5", hub(1, 4, Speed::High).0).unwrap();
        attach(&mut tree, "6", device(2, Speed::Full)).unwrap();

        assert_eq!(
            attach(&mut tree, "5", device(3, Speed::High)),
            Err(Response::PathOccupied)
        );
        assert_eq!(
            attach(&mut tree, "9", device(3, Speed::High)),
            Err(Response::NoSuchPath),
            "there are 8 root ports"
        );
        assert_eq!(
            attach(&mut tree, "7.1", device(3, Speed::High)),
            Err(Response::NoSuchPath),
            "nothing on root port 7"
        );
        assert_eq!(
            attach(&mut tree, "6.1", device(3, Speed::High)),
            Err(Response::NotAHub)
        );
        assert_eq!(
            attach(&mut tree, "5.5", device(3, Speed::High)),
            Err(Response::NoSuchPath),
            "the hub has 4 ports"
        );
        assert_eq!(
            attach(&mut tree, "5.1.1", device(3, Speed::High)),
            Err(Response::NoSuchPath),
            "nothing on port 1"
        );
        attach(&mut tree, "5.4", device(3, Speed::High)).unwrap();
        assert_eq!(
            attach(&mut tree, "5.4", device(4, Speed::High)),
            Err(Response::PathOccupied)
        );
        assert_eq!(tree.nodes().len(), 3);
    }

    #[test]
    fn a_high_speed_device_is_refused_below_a_full_speed_hub() {
        let mut tree = DeviceTree::new();
        attach(&mut tree, "5", hub(1, 4, Speed::Full).0).unwrap();

        assert_eq!(
            attach(&mut tree, "5.1", device(2, Speed::High)),
            Err(Response::SpeedNotSupported)
        );
        assert_eq!(
            attach(&mut tree, "5.1", hub(3, 4, Speed::High).0),
            Err(Response::SpeedNotSupported),
            "a high speed hub is a high speed device"
        );
        assert_eq!(
            attach(&mut tree, "5.1", device(4, Speed::Super)),
            Err(Response::SpeedNotSupported)
        );
        attach(&mut tree, "5.1", device(5, Speed::Full)).unwrap();
        attach(&mut tree, "5.2", device(6, Speed::Low)).unwrap();
        // A full speed hub below a high speed one is fine.
        let mut tree = DeviceTree::new();
        attach(&mut tree, "5", hub(1, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.1", hub(2, 4, Speed::Full).0).unwrap();
        attach(&mut tree, "5.1.1", device(3, Speed::Full)).unwrap();
        assert_eq!(
            attach(&mut tree, "5.1.2", device(4, Speed::High)),
            Err(Response::SpeedNotSupported)
        );
    }

    #[test]
    fn detaching_a_hub_takes_the_subtree_and_cancels_every_device() {
        let mut tree = DeviceTree::new();
        let tokens: Vec<_> = [
            ("5", hub(1, 4, Speed::High).0),
            ("5.1", hub(2, 4, Speed::High).0),
            ("5.1.2", device(3, Speed::Full)),
            ("5.3", device(4, Speed::High)),
            ("6", device(5, Speed::Full)),
        ]
        .into_iter()
        .map(|(at, device)| {
            let token = device.detach_token();
            attach(&mut tree, at, device).unwrap();
            token
        })
        .collect();

        let node = tree.detach(&path("5")).unwrap();
        node.cancel_detach_tokens();

        for token in &tokens[..4] {
            assert!(token.is_cancelled());
        }
        assert!(
            !tokens[4].is_cancelled(),
            "the device on another port stays"
        );
        assert_eq!(ids(&tree), [("6".to_string(), DeviceId::Virtual(5))]);
        assert!(tree.detach(&path("5")).is_none());
    }

    #[test]
    fn detaching_a_device_below_a_hub_leaves_its_neighbours() {
        let mut tree = DeviceTree::new();
        attach(&mut tree, "5", hub(1, 4, Speed::High).0).unwrap();
        attach(&mut tree, "5.1", device(2, Speed::Full)).unwrap();
        attach(&mut tree, "5.2", device(3, Speed::Full)).unwrap();

        let node = tree.detach(&path("5.1")).unwrap();

        assert_eq!(node.device.identifier(), DeviceId::Virtual(2));
        assert_eq!(
            ids(&tree)
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            ["5", "5.2"]
        );
        // The path can be used again.
        attach(&mut tree, "5.1", device(4, Speed::High)).unwrap();
    }

    #[test]
    fn the_hub_on_the_way_learns_about_devices_coming_and_going() {
        let mut tree = DeviceTree::new();
        let (outer, outer_ports) = hub(1, 4, Speed::High);
        let (inner, inner_ports) = hub(2, 4, Speed::High);
        attach(&mut tree, "5", outer).unwrap();
        for port in 1..=4 {
            outer_ports
                .set_port_feature(port, port_feature::POWER)
                .unwrap();
            inner_ports
                .set_port_feature(port, port_feature::POWER)
                .unwrap();
        }

        attach(&mut tree, "5.1", inner).unwrap();
        attach(&mut tree, "5.1.4", device(3, Speed::Full)).unwrap();

        let outer_port = outer_ports.port_status(1).unwrap();
        assert_eq!(
            u16::from_le_bytes([outer_port[0], outer_port[1]]) & port_status::CONNECTION,
            port_status::CONNECTION
        );
        assert_eq!(outer_ports.status_change_bitmap(), Some(vec![1 << 1]));
        assert_eq!(inner_ports.status_change_bitmap(), Some(vec![1 << 4]));

        // The device of the inner hub is removed, and the inner hub itself.
        for port in 1..=4 {
            inner_ports
                .clear_port_feature(port, port_feature::C_CONNECTION)
                .unwrap();
        }
        assert!(tree.detach(&path("5.1.4")).is_some());
        let removed = inner_ports.port_status(4).unwrap();
        assert_eq!(removed[0] & 1, 0);
        assert_eq!(
            u16::from_le_bytes([removed[2], removed[3]]),
            port_change::CONNECTION
        );

        assert!(tree.detach(&path("5.1")).is_some());
        let removed = outer_ports.port_status(1).unwrap();
        assert_eq!(removed[0] & 1, 0);
    }

    #[test]
    fn resets_reach_the_hubs() {
        let mut tree = DeviceTree::new();
        let (outer, outer_ports) = hub(1, 4, Speed::High);
        let (other, other_ports) = hub(2, 4, Speed::High);
        attach(&mut tree, "5", outer).unwrap();
        attach(&mut tree, "6", other).unwrap();
        outer_ports.set_configuration(1);
        other_ports.set_configuration(1);

        tree.reset_hub_on_root_port(5);
        assert_eq!(outer_ports.configuration(), 0);
        assert_eq!(other_ports.configuration(), 1);

        outer_ports.set_configuration(1);
        tree.reset_hubs();
        assert_eq!(outer_ports.configuration(), 0);
        assert_eq!(other_ports.configuration(), 0);
        tree.reset_hub_on_root_port(1);
        tree.reset_hub_on_root_port(200);
    }

    #[test]
    fn root_ports_are_free_until_a_device_is_attached() {
        let mut tree = DeviceTree::new();
        assert!(tree.root_port_is_free(8));
        assert!(!tree.root_port_is_free(0));
        assert!(!tree.root_port_is_free(9));

        attach(&mut tree, "8", device(1, Speed::Full)).unwrap();

        assert!(!tree.root_port_is_free(8));
    }
}
