#![deny(
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::must_use_candidate
)]
// now allow a few rules which are denied by the above's statement
#![allow(clippy::multiple_crate_versions)]
#![deny(missing_debug_implementations)]
#![deny(rustdoc::all)]

//! Command-line tool to attach/detach/list USB devices to/from usbvfiod at runtime.

use std::{
    fs::File,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgAction, Parser};
use nusb::MaybeFuture;
use usbvfiod::hotplug_protocol::{
    command::Command,
    device_paths::resolve_path,
    hex,
    port_path::PortPath,
    reply::Reply,
    response::Response,
    spec::{HubSpec, UsbSpeed},
};

fn main() -> Result<()> {
    let args = Cli::parse();
    let socket = args.socket.as_path();

    if let Some(path) = &args.attach {
        attach(path.as_path(), socket)?;
    } else if let Some(vec) = &args.detach {
        // Safety: clap ensures that vec.len() == 2.
        let bus = vec[0];
        let dev = vec[1];
        detach(bus, dev, socket)?;
    } else if args.list {
        list_attached(socket)?;
    } else if let Some(path) = args.hub {
        attach_hub(path, &args)?;
    } else if let Some(path) = args.attach_at {
        let device = args
            .device
            .as_deref()
            .context("--attach-at needs the device with --device /dev/bus/usb/BBB/DDD")?;
        attach_at(path, device, socket)?;
    } else if let Some(path) = args.virtual_hid {
        let speed = args
            .speed
            .context("--virtual-hid needs the speed with --speed full or --speed high")?;
        ask(socket, Command::AttachVirtualHid { path, speed })
            .and_then(|reply| report_attach(&reply, path))?;
    } else if let Some(path) = args.detach_at {
        println!("Requesting detach of the device at {path}");
        let reply = ask(socket, Command::DetachAt { path })?;
        report(&reply)?;
    } else if args.tree {
        tree(socket, args.json)?;
    }

    Ok(())
}

/// Open the device file of a USB device, reset the device and open the file again, which is what
/// the controller gets as the starting point of a device.
fn open_reset_device(device_path: &Path) -> Result<(u8, u8, File)> {
    let (bus, dev, device_path) = resolve_path(device_path)
        .with_context(|| format!("Failed to resolve device path {device_path:?}"))?;

    println!("Requesting attachment of device {bus:03}:{dev:03}");

    let open_file = |err_msg: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path)
            .with_context(|| err_msg.to_string())
    };

    let file = open_file("Failed to open USB device file")?;
    let device = nusb::Device::from_fd(file.into())
        .wait()
        .context("Failed to open nusb device")?;
    device.reset().wait().context("Failed to reset device")?;

    // After the reset, the device instance is no longer usable and we need
    // to reopen.
    let file = open_file("Failed to open USB device file after device reset")?;
    Ok((bus, dev, file))
}

/// Send a framed command and read the reply.
fn ask(socket_path: &Path, command: Command) -> Result<Reply> {
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send the command over the socket")?;
    Reply::receive_from_socket(&mut socket).context("Failed to receive the reply over the socket")
}

/// Print the outcome of a command, and fail if it did not work.
fn report(reply: &Reply) -> Result<()> {
    println!("{:?}", reply.response);
    if reply.response == Response::SuccessfulOperation {
        return Ok(());
    }
    match &reply.message {
        Some(message) => bail!("{:?}: {message}", reply.response),
        None => bail!("{:?}", reply.response),
    }
}

fn report_attach(reply: &Reply, path: PortPath) -> Result<()> {
    println!("Requested attachment at {path}");
    report(reply)
}

fn attach_at(path: PortPath, device_path: &Path, socket_path: &Path) -> Result<()> {
    let (bus, device, fd) = open_reset_device(device_path)?;
    let reply = ask(
        socket_path,
        Command::AttachAt {
            path,
            bus,
            device,
            fd,
        },
    )?;
    report_attach(&reply, path)
}

fn attach_hub(path: PortPath, args: &Cli) -> Result<()> {
    let descriptors = args
        .descriptors
        .as_deref()
        .map(|file| -> Result<String> {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("Failed to read the descriptors from {file:?}"))?;
            hex::parse(&text)
                .with_context(|| format!("The descriptors in {file:?} are not hexadecimal text"))?;
            Ok(text)
        })
        .transpose()?;
    if descriptors.is_none() && (args.vid.is_none() || args.pid.is_none()) {
        bail!("--hub needs --vid and --pid, or --descriptors");
    }
    let spec = HubSpec {
        vid: args.vid,
        pid: args.pid,
        bcd_device: args.bcd_device,
        manufacturer: args.manufacturer.clone(),
        product: args.product.clone(),
        serial: args.serial.clone(),
        ports: args
            .ports
            .context("--hub needs the number of downstream ports with --ports")?,
        speed: args
            .speed
            .context("--hub needs the speed with --speed full or --speed high")?,
        descriptors,
    };

    let reply = ask(&args.socket, Command::AttachHub { path, spec })?;
    report_attach(&reply, path)
}

fn tree(socket_path: &Path, json: bool) -> Result<()> {
    let reply = ask(socket_path, Command::Tree)?;
    if reply.response != Response::SuccessfulOperation {
        return report(&reply);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }

    if reply.tree.is_empty() {
        println!("No attached devices");
    }
    for entry in &reply.tree {
        let ports = entry
            .hub_ports
            .map_or_else(String::new, |ports| format!("hub, {ports} ports"));
        // The depth shows as indentation, so that the tree is easy to see.
        let indent = "  ".repeat(entry.path.hub_ports().len());
        println!(
            "{indent}{:<width$} {:<5} {:<14} {}",
            entry.path.to_string(),
            entry.speed,
            ports,
            entry.id,
            width = 18 - indent.len().min(10),
        );
    }
    Ok(())
}

fn attach(device_path: &Path, socket_path: &Path) -> Result<()> {
    let (bus, dev, file) = open_reset_device(device_path)?;

    // write to socket for hot-attach fds
    let command = Command::Attach {
        bus,
        device: dev,
        fd: file,
    };
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send attach command over the socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;
    println!("{response:?}");

    Ok(())
}

fn detach(bus: u8, dev: u8, socket_path: &Path) -> Result<()> {
    println!("Requesting detach of device {bus:03}:{dev:03}");

    let command = Command::Detach { bus, device: dev };
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send detach command over the socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;
    println!("{response:?}");

    Ok(())
}

fn list_attached(socket_path: &Path) -> Result<()> {
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    Command::List
        .send_over_socket(&socket)
        .context("Failed to send list command over socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;

    if response != Response::ListFollowing {
        return Err(anyhow!(
            "Expected the response {:?} but got {:?}",
            Response::ListFollowing,
            response
        ));
    }

    let device_list = response.receive_devices_list(&mut socket)?;
    match device_list.len() {
        0 => println!("No attached devices"),
        1 => {
            println!("One attached device:");
            println!("{:03}:{:03}", device_list[0].0, device_list[0].1);
        }
        count => {
            println!("{count} attached devices:");
            for (bus, dev) in device_list {
                println!("{bus:03}:{dev:03}");
            }
        }
    }

    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
    about = env!("CARGO_PKG_DESCRIPTION"),
    long_about = None,
    group(
        clap::ArgGroup::new("action")
            .args(["attach", "detach", "list", "hub", "attach_at", "virtual_hid", "detach_at", "tree"])
            .multiple(false)
    )
)]
struct Cli {
    /// Path to the hot-attach socket that the usbvfiod instances exposes.
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,

    /// Attach the USB device to usbvfiod. The path must point to a device in: /dev/bus/usb.
    /// This option is mutually exclusive with the other actions.
    #[arg(long, value_name = "PATH")]
    attach: Option<PathBuf>,

    /// Detach the USB device from usbvfiod. Specify the device with the bus number
    /// and the device number.
    ///
    /// This option is mutually exclusive with the other actions.
    #[arg(long, num_args = 2)]
    detach: Option<Vec<u8>>,

    /// List the currently attached USB devices.
    ///
    /// This option is mutually exclusive with the other actions.
    #[arg(long, action = ArgAction::SetTrue)]
    list: bool,

    /// Attach a virtual USB 2.0 hub at a port path, which is `R` for the root port R (in the
    /// numbering of the PORTSC registers, USB 2 ports start after the USB 3 ports) and
    /// `R.p1.p2` for port p2 of the hub on port p1 of the hub on the root port R.
    ///
    /// Describe the hub with --ports and --speed, and with --vid and --pid or --descriptors.
    #[arg(long, value_name = "PATH")]
    hub: Option<PortPath>,

    /// Attach the USB device given with --device at a port path. The hubs on the way must exist.
    #[arg(long, value_name = "PATH")]
    attach_at: Option<PortPath>,

    /// Attach the virtual HID test device at a port path, with the speed of --speed.
    #[arg(long, value_name = "PATH")]
    virtual_hid: Option<PortPath>,

    /// Detach the device at a port path, with the devices below it if it is a hub.
    #[arg(long, value_name = "PATH")]
    detach_at: Option<PortPath>,

    /// Show the attached devices as a tree with their port paths.
    #[arg(long, action = ArgAction::SetTrue)]
    tree: bool,

    /// The device for --attach-at. The path must point to a device in: /dev/bus/usb.
    #[arg(long, value_name = "PATH", requires = "attach_at")]
    device: Option<PathBuf>,

    /// The speed of the hub of --hub, or of the device of --virtual-hid: full or high.
    #[arg(long, value_name = "SPEED")]
    speed: Option<UsbSpeed>,

    /// The number of downstream ports of the hub of --hub, 1 to 15.
    #[arg(long, value_name = "N", requires = "hub")]
    ports: Option<u8>,

    /// The vendor ID of the hub of --hub in hexadecimal.
    #[arg(long, value_name = "HEX", value_parser = parse_hex_u16, requires = "hub")]
    vid: Option<u16>,

    /// The product ID of the hub of --hub in hexadecimal.
    #[arg(long, value_name = "HEX", value_parser = parse_hex_u16, requires = "hub")]
    pid: Option<u16>,

    /// The device release number (bcdDevice) of the hub of --hub in hexadecimal.
    #[arg(long, value_name = "HEX", value_parser = parse_hex_u16, requires = "hub")]
    bcd_device: Option<u16>,

    /// The manufacturer string of the hub of --hub.
    #[arg(long, value_name = "TEXT", requires = "hub")]
    manufacturer: Option<String>,

    /// The product string of the hub of --hub.
    #[arg(long, value_name = "TEXT", requires = "hub")]
    product: Option<String>,

    /// The serial number string of the hub of --hub.
    #[arg(long, value_name = "TEXT", requires = "hub")]
    serial: Option<String>,

    /// Serve the device and configuration descriptor of the hub of --hub from this file of
    /// hexadecimal text (the device descriptor first, then the configuration descriptor) instead
    /// of generated ones. The strings still come from the options above.
    #[arg(long, value_name = "FILE", requires = "hub")]
    descriptors: Option<PathBuf>,

    /// Print the reply to --tree as JSON.
    #[arg(long, action = ArgAction::SetTrue, requires = "tree")]
    json: bool,
}

/// Read a 16-bit number in hexadecimal, with or without the prefix 0x.
fn parse_hex_u16(text: &str) -> Result<u16, std::num::ParseIntError> {
    u16::from_str_radix(text.trim_start_matches("0x"), 16)
}
