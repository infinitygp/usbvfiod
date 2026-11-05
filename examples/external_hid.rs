//! An external device server for manual checks: a full speed HID device with a vendor defined
//! input report that serves every connection it accepts over USB/IP framing, as described in
//! `docs/users/external-devices.md`. Its device descriptor says USB 1.10 and 8 bytes for endpoint
//! 0, like many full speed HID devices.
//!
//! ```console
//! $ cargo run --example external_hid -- --listen $PWD/D
//! $ remote --socket H --external 5.1.1.4 --speed full --connect $PWD/D --name hid
//! ```
//!
//! Every `remote --external` makes one device. A report is sent four times a second while the
//! guest waits for one. With `--close-after SECS` the server closes a connection after that many
//! seconds, which the guest sees as unplugging the device.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use usbvfiod::usbip::{
    CmdSubmit, CmdUnlink, Direction, Header, RetSubmit, RetUnlink, HEADER_LEN, STATUS_CANCELLED,
    STATUS_STALL,
};

#[derive(Parser, Debug)]
struct Cli {
    /// The socket to listen on. A device is served on every connection.
    #[arg(long, value_name = "PATH")]
    listen: PathBuf,

    /// Close a connection after this many seconds.
    #[arg(long, value_name = "SECS")]
    close_after: Option<u64>,

    /// The time between two reports, in milliseconds.
    #[arg(long, value_name = "MS", default_value_t = 250)]
    report_interval: u64,
}

const REPORT_DESCRIPTOR: [u8; 21] = [
    0x06, 0x00, 0xff, // Usage Page (vendor defined)
    0x09, 0x01, // Usage (1)
    0xa1, 0x01, // Collection (Application)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xff, 0x00, //   Logical Maximum (255)
    0x75, 0x08, //   Report Size (8)
    0x95, 0x02, //   Report Count (2)
    0x09, 0x02, //   Usage (2)
    0x81, 0x02, //   Input (Data, Variable, Absolute)
    0xc0, // End Collection
];

const DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, // bLength, bDescriptorType
    0x10, 0x01, // bcdUSB 1.10
    0, 0, 0, // class, subclass, protocol: defined by the interface
    8, // bMaxPacketSize0
    0x09, 0x12, // idVendor 0x1209
    0x01, 0xde, // idProduct 0xde01
    0x00, 0x01, // bcdDevice 1.00
    1, 2, 3, // manufacturer, product, serial number
    1, // bNumConfigurations
];

const CONFIGURATION_DESCRIPTOR: [u8; 34] = [
    9, 2, 34, 0, 1, 1, 0, 0x80, 50, // configuration with one interface
    9, 4, 0, 0, 1, 3, 0, 0, 0, // interface 0: HID with one endpoint
    9, 0x21, 0x11, 0x01, 0, 1, 0x22, 21,
    0, // HID descriptor, the report descriptor has 21 bytes
    7, 5, 0x81, 3, 2, 0, 10, // endpoint 1 IN, interrupt, 2 bytes, every 10 ms
];

fn string_descriptor(index: u8) -> Option<Vec<u8>> {
    let text = match index {
        0 => return Some(vec![4, 3, 0x09, 0x04]),
        1 => "usbvfiod",
        2 => "usbvfiod external HID",
        3 => "0001",
        _ => return None,
    };
    let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut descriptor = vec![u8::try_from(units.len() + 2).ok()?, 3];
    descriptor.extend(units);
    Some(descriptor)
}

/// What a control request on endpoint 0 returns, or `None` to stall.
fn control(setup: [u8; 8]) -> Option<Vec<u8>> {
    let (request_type, request) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let [descriptor_type, descriptor_index] = [value.to_be_bytes()[0], value.to_le_bytes()[0]];

    match (request_type, request) {
        (0x80, 6) => match descriptor_type {
            1 => Some(DEVICE_DESCRIPTOR.to_vec()),
            2 => Some(CONFIGURATION_DESCRIPTOR.to_vec()),
            3 => string_descriptor(descriptor_index),
            _ => None,
        },
        (0x81, 6) if descriptor_type == 0x22 => Some(REPORT_DESCRIPTOR.to_vec()),
        (0x80..=0x82, 0) => Some(vec![0, 0]),
        // SET_CONFIGURATION, SET_INTERFACE, CLEAR_FEATURE, SET_IDLE and SET_PROTOCOL.
        (0x00..=0x02, 9 | 11 | 1) | (0x21, 0x0a | 0x0b) => Some(Vec::new()),
        _ => None,
    }
}

/// An interrupt IN transfer that waits for a report.
#[derive(Debug)]
struct Waiting {
    seqnum: u32,
    length: u32,
}

fn send(writer: &Mutex<UnixStream>, header: Header, data: &[u8]) -> io::Result<()> {
    let mut message = header.encode().to_vec();
    message.extend_from_slice(data);
    let mut writer = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    writer.write_all(&message)
}

fn complete(writer: &Mutex<UnixStream>, seqnum: u32, status: i32, data: &[u8]) -> io::Result<()> {
    let actual_length = i32::try_from(data.len()).map_err(io::Error::other)?;
    send(
        writer,
        Header::RetSubmit(RetSubmit {
            seqnum,
            status,
            actual_length,
        }),
        data,
    )
}

fn handle_submit(
    stream: &mut UnixStream,
    writer: &Mutex<UnixStream>,
    waiting: &Mutex<VecDeque<Waiting>>,
    submit: CmdSubmit,
) -> io::Result<()> {
    let out_data = match submit.direction {
        Direction::In => Vec::new(),
        Direction::Out => {
            let mut data = vec![0u8; submit.transfer_buffer_length as usize];
            stream.read_exact(&mut data)?;
            data
        }
    };

    match (submit.ep, submit.direction) {
        (0, direction) => {
            eprintln!(
                "ep 0 {direction:?} setup {:02x?} data {out_data:02x?}",
                submit.setup
            );
            match control(submit.setup) {
                Some(mut data) => {
                    data.truncate(submit.transfer_buffer_length as usize);
                    match direction {
                        Direction::In => complete(writer, submit.seqnum, 0, &data),
                        // The data stage of an OUT request is accepted as a whole.
                        Direction::Out => send(
                            writer,
                            Header::RetSubmit(RetSubmit {
                                seqnum: submit.seqnum,
                                status: 0,
                                actual_length: i32::try_from(out_data.len())
                                    .map_err(io::Error::other)?,
                            }),
                            &[],
                        ),
                    }
                }
                None => complete(writer, submit.seqnum, STATUS_STALL, &[]),
            }
        }
        (1, Direction::In) => {
            waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(Waiting {
                    seqnum: submit.seqnum,
                    length: submit.transfer_buffer_length,
                });
            Ok(())
        }
        _ => complete(writer, submit.seqnum, STATUS_STALL, &[]),
    }
}

fn handle_unlink(
    writer: &Mutex<UnixStream>,
    waiting: &Mutex<VecDeque<Waiting>>,
    unlink: CmdUnlink,
) -> io::Result<()> {
    let cancelled = {
        let mut waiting = waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = waiting.len();
        waiting.retain(|transfer| transfer.seqnum != unlink.unlink_seqnum);
        waiting.len() != before
    };
    eprintln!(
        "unlink {}: {}",
        unlink.unlink_seqnum,
        if cancelled {
            "cancelled"
        } else {
            "completed already"
        }
    );
    send(
        writer,
        Header::RetUnlink(RetUnlink {
            seqnum: unlink.seqnum,
            status: if cancelled { STATUS_CANCELLED } else { 0 },
        }),
        &[],
    )
}

/// Answer the waiting interrupt transfers with a report each, at the interval.
fn send_reports(
    writer: &Mutex<UnixStream>,
    waiting: &Mutex<VecDeque<Waiting>>,
    interval: Duration,
    closed: &AtomicBool,
) {
    let mut counter = 0u8;
    while !closed.load(Ordering::Relaxed) {
        thread::sleep(interval);
        let transfer = waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if let Some(transfer) = transfer {
            counter = counter.wrapping_add(1);
            let mut report = vec![counter, !counter];
            report.truncate(transfer.length as usize);
            if complete(writer, transfer.seqnum, 0, &report).is_err() {
                return;
            }
        }
    }
}

fn serve(
    mut stream: UnixStream,
    report_interval: Duration,
    close_after: Option<Duration>,
) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let waiting = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(AtomicBool::new(false));

    let reports = {
        let (writer, waiting, closed) = (writer.clone(), waiting.clone(), closed.clone());
        thread::spawn(move || send_reports(&writer, &waiting, report_interval, &closed))
    };
    if let Some(after) = close_after {
        let stream = stream.try_clone()?;
        let started = Instant::now();
        thread::spawn(move || {
            thread::sleep(after.saturating_sub(started.elapsed()));
            eprintln!("closing the connection");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
    }

    let result = loop {
        let mut header = [0u8; HEADER_LEN];
        if let Err(err) = stream.read_exact(&mut header) {
            break if err.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(err)
            };
        }
        let handled = match Header::decode(&header) {
            Ok(Header::CmdSubmit(submit)) => handle_submit(&mut stream, &writer, &waiting, submit),
            Ok(Header::CmdUnlink(unlink)) => handle_unlink(&writer, &waiting, unlink),
            Ok(other) => Err(io::Error::other(format!("unexpected {other}"))),
            Err(err) => Err(io::Error::other(err)),
        };
        if let Err(err) = handled {
            break Err(err);
        }
    };

    closed.store(true, Ordering::Relaxed);
    let _ = reports.join();
    result
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let _ = std::fs::remove_file(&cli.listen);
    let listener = UnixListener::bind(&cli.listen)?;
    eprintln!("listening on {}", cli.listen.display());

    let report_interval = Duration::from_millis(cli.report_interval);
    let close_after = cli.close_after.map(Duration::from_secs);
    for stream in listener.incoming() {
        let stream = stream?;
        eprintln!("a device is attached");
        thread::spawn(move || match serve(stream, report_interval, close_after) {
            Ok(()) => eprintln!("the connection ended"),
            Err(err) => eprintln!("the connection failed: {err}"),
        });
    }
    Ok(())
}
