//! Passthrough of individual USB devices using the vfio-user protocol

mod async_runtime;
mod cli;
mod device;
mod dynamic_bus;
mod hotplug_server;
mod memory_segment;
mod one_indexed_array;
mod oneshot_anyhow;
mod xhci_backend;

use std::{
    os::fd::{FromRawFd, OwnedFd},
    path::PathBuf,
    sync::Once,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_runtime::init_runtime;
use clap::Parser;
use cli::Cli;
use device::pcap::UsbPcapManager;
use hotplug_server::run_hotplug_server;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use tokio::runtime::Handle;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;

use crate::{
    async_runtime::runtime,
    device::xhci::{
        any_device::ControllerDevice, nusb::claimed_host_devices, port::HotplugControl,
    },
};

/// How long the host devices have to go back to the host when usbvfiod ends.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a cancelled transfer may hold its interface after its device was dropped.
const RELEASE_SETTLE: Duration = Duration::from_millis(200);

static RELEASE: Once = Once::new();

/// Gives the host devices back to the host before the process ends: detaches every device and
/// waits until the interfaces of the host devices are released, which binds their kernel drivers
/// again. A process that just ends leaves them without a driver. The first caller does the work,
/// a second one waits for it.
fn release_devices(control: &HotplugControl<ControllerDevice>, async_runtime: &Handle) {
    RELEASE.call_once(|| {
        let held = claimed_host_devices();
        async_runtime.block_on(async {
            control.detach_all().await;
            let deadline = Instant::now() + RELEASE_TIMEOUT;
            while claimed_host_devices() > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if held > 0 {
                tokio::time::sleep(RELEASE_SETTLE).await;
            }
        });
        match claimed_host_devices() {
            0 => info!("Gave {held} host devices back to the host"),
            left => warn!("{left} of {held} host devices were not released in time"),
        }
    });
}

fn remove_hotplug_socket(path: Option<&PathBuf>) {
    if let Some(path) = path.filter(|path| path.exists()) {
        let _ = std::fs::remove_file(path);
    }
}

fn main() -> Result<()> {
    let args = Cli::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(match args.verbose {
            0 => Level::INFO,
            1 => Level::DEBUG,
            _ => Level::TRACE,
        })
        .with_ansi(!args.no_color)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set global tracing subscriber")?;

    // Log messages from the log crate as well.
    tracing_log::LogTracer::init()?;

    UsbPcapManager::init(args.pcap_path.clone());

    init_runtime().context("Failed to initialize async runtime")?;
    let runtime = runtime();

    let mut backend = xhci_backend::XhciBackend::new(runtime.clone())
        .context("Failed to create virtual XHCI controller")?;
    for device in &args.devices {
        let path = device.as_path();
        // if initial device attachment fails, make it clear by panicking
        if let Err(err) = runtime.block_on(backend.add_device_from_path(path, runtime.clone())) {
            panic!("Device attachment failed for {path:?}: {err}");
        }
    }

    let server = match args.server_socket() {
        cli::ServerSocket::Path(socket_path) => {
            Server::new(socket_path, true, backend.irqs(), backend.regions())
                .context("Failed to create vfio-user server")?
        }
        cli::ServerSocket::Fd(fd) => {
            // SAFETY: we have to assume the given fd is valid, there is not much else we can do
            let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Server::from_owned_fd(owned_fd, true, backend.irqs(), backend.regions())
        }
    };

    if let Some(socket) = args.hotplug_socket() {
        let hotplug_control = backend.hotplug_control();
        thread::Builder::new()
            .name("hot-attach-socket listener".to_string())
            .spawn(move || run_hotplug_server(socket, hotplug_control, runtime.clone()))
            .unwrap();
    }

    let control = backend.hotplug_control();
    let mut signals =
        Signals::new([SIGINT, SIGTERM]).context("Failed to set up the signal handler")?;
    {
        let (control, socket_path) = (backend.hotplug_control(), args.hotplug_socket_path.clone());
        let runtime = runtime.clone();
        thread::Builder::new()
            .name("signals".to_string())
            .spawn(move || {
                if let Some(signal) = signals.forever().next() {
                    info!("Ending on signal {signal}");
                    release_devices(&control, &runtime);
                    remove_hotplug_socket(socket_path.as_ref());
                    std::process::exit(128 + signal);
                }
            })
            .context("Failed to start the signal thread")?;
    }

    info!("We're up!");

    let outcome = loop {
        let result = server.run(&mut backend);

        if !args.reconnect {
            break result.context("Failed to start vfio-user server");
        }

        match result {
            Ok(()) => info!("The vfio-user client disconnected"),
            Err(err) => warn!("The vfio-user connection failed: {err}"),
        }
        backend.disconnect();
        info!("Waiting for the next vfio-user client");
    };

    release_devices(&control, runtime);
    remove_hotplug_socket(args.hotplug_socket_path.as_ref());

    outcome
}
