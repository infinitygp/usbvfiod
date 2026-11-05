# External Devices

An external device is a USB device that another process emulates. `usbvfiod` presents it to the
guest at a port path (see [Hubs and Port Paths](./hubs.md)), forwards every transfer of the guest to
the process over a unix stream socket, and hands the answers back. The framing of the socket is
that of USB/IP. The guest sees exactly the descriptors that the process serves, down to the USB
version and the packet size of endpoint 0, and the host needs neither a kernel gadget nor root
rights.

The pieces:

```
guest  <-- xHCI -->  usbvfiod  <-- unix stream socket, USB/IP framing -->  device server
                        ^
                        |  hotplug socket: command 0x15 and a file descriptor
                      client
```

The client that attaches the device is usually the device server itself. It creates a connected
socket pair, keeps one end, and sends the other end to `usbvfiod` with the command 0x15.

## Attaching a device

The command 0x15 uses the framing of the other hotplug commands (see
[Hubs and Port Paths](./hubs.md#the-protocol-of-the-hotplug-socket)): the command byte, the length of
the payload as 32-bit little endian number, and the payload as JSON. The reply is a 32-bit little
endian length and a JSON object with the `response` and an optional `message`.

```json
{"path": "5.1.1.4", "speed": "full", "name": "Test keypad"}
```

| Field | Meaning |
|---|---|
| `path` | the port path of the device, which has to be free, below hubs that exist |
| `speed` | `low`, `full` or `high` |
| `name` | optional, a name for the log and for the tree listing |

The message carries exactly one file descriptor with `SCM_RIGHTS`: one end of a connected `AF_UNIX`
`SOCK_STREAM` socket. Send the whole command, byte, length and payload, in one `sendmsg` with the
descriptor attached. `usbvfiod` reads the first byte of the message to get the descriptor and then
the rest.

`usbvfiod` checks the command like the other attach commands. The responses are `SuccessfulOperation`,
`PathOccupied`, `NoSuchPath` (the root port, the hub on the way or its port does not exist),
`NotAHub`, `SpeedNotSupported` (a high speed device below a full speed hub), `InvalidRequest` (a speed
that is not `low`, `full` or `high`) and `FailedToOpenFd` (the descriptor is not a connected unix
socket). `usbvfiod` closes the socket when it does not attach the device, which the peer sees as the
end of the stream.

`remote --socket H --tree` lists the device as `external NAME`, or as `external #N` without a name.
`remote --socket H --detach-at PATH` detaches it like any other device.

### Lifetime

- The device exists as long as the socket does. The guest sees a device that is plugged in when the
  command succeeds, and unplugged when the device is detached.
- `detach-at` for the path (or for a hub above it) detaches the device and shuts the socket down
  (`SHUT_RDWR`) and closes it. The peer sees the end of the stream. Nothing is sent before that:
  transfers in flight are not cancelled with `CMD_UNLINK`.
- When the peer closes the socket, or sends something that is not valid framing (see below),
  `usbvfiod` detaches the device by itself, and the guest sees the device unplugged. Transfers in
  flight end with a transaction error. `usbvfiod` shuts the socket down in this case, too, so that
  the peer sees the end of the stream even if it still holds another copy of its end.
- A device that is attached again, with a new socket, is a new device for the guest.

Serve the socket before sending the command. The guest can start to enumerate the device as soon as
the command succeeded, and the first transfer can reach the socket before the client has read the
reply.

## The socket

The messages are the `CMD_SUBMIT`, `RET_SUBMIT`, `CMD_UNLINK` and `RET_UNLINK` of USB/IP, as
described in the Linux kernel's `Documentation/usb/usbip_protocol.rst`. A header has 48 bytes, and
all fields are big-endian. There is no `OP_REQ_DEVLIST` or `OP_REQ_IMPORT` phase: the identity of the
device comes from the descriptors that the device serves, and the speed from the hotplug command.
`usbvfiod::usbip` is a Rust implementation of the headers that `usbvfiod` and `examples/external_hid.rs`
share.

### From `usbvfiod` to the device

**`CMD_SUBMIT`** (command `0x00000001`) asks for a transfer.

| Field | Value |
|---|---|
| `seqnum` | counts from 1 for each device and is unique among the transfers in flight; `CMD_UNLINK` uses the same counter |
| `devid` | 0 |
| `direction` | 0 for OUT, 1 for IN |
| `ep` | the endpoint number, 0 to 15, without the direction bit |
| `transfer_flags` | 0 |
| `transfer_buffer_length` | the length of the buffer of the transfer |
| `start_frame`, `number_of_packets`, `interval` | 0 |
| `setup` | the setup packet for a control transfer on endpoint 0, zeros otherwise |

For an OUT transfer, `transfer_buffer_length` bytes of data follow the header. For an IN transfer,
the device may return up to `transfer_buffer_length` bytes. A transfer on endpoint 0 is a control
transfer. Its direction is bit 7 of `bmRequestType` in the setup packet, `transfer_buffer_length` is
`wLength`, and the data stage is the OUT data or the IN answer. A control request without a data
stage has `transfer_buffer_length` 0. For a bulk or interrupt endpoint, `transfer_buffer_length` is
the length of the transfer descriptor that the guest queued, which can be longer than the maximum
packet size of the endpoint. A transfer shorter than that completes with a short packet.

`usbvfiod` forwards every control request of the guest to the device, the standard requests
(`GET_DESCRIPTOR`, `SET_CONFIGURATION`, `SET_INTERFACE`, `CLEAR_FEATURE` for `ENDPOINT_HALT`,
`GET_STATUS`, ...) as well. It never answers a request itself. It does not send `SET_ADDRESS`: the
address of the device is handled by the controller. The device chooses the descriptors it serves.
The endpoint types and numbers come from the configuration the guest selects, as for any device.
Isochronous endpoints are not supported: the controller refuses to configure them.

At most one transfer per endpoint is in flight, and the control endpoint is an endpoint with one
transfer at a time. Transfers of different endpoints are in flight at the same time: an interrupt IN
transfer waits until the device has data to return, while control requests on endpoint 0 keep being
served. `usbvfiod` sets no time limit on transfers. A transfer that the device does not complete
stays in flight until the guest stops the endpoint.

**`CMD_UNLINK`** (command `0x00000002`) cancels a transfer. It has a new `seqnum` of its own, the
`unlink_seqnum` of the `CMD_SUBMIT` to cancel, and 24 bytes of padding. `usbvfiod` sends it when
the guest stops an endpoint that has a transfer in flight (Stop Endpoint). It waits for the answer
for one second. A device that does not answer in this time is treated as broken and detached. A new
`CMD_SUBMIT` for the endpoint follows when the guest queues the next transfer, with a new `seqnum`.
It also sends a `CMD_UNLINK` without waiting for the answer when it drops an endpoint that has a
transfer in flight.

### From the device to `usbvfiod`

**`RET_SUBMIT`** (command `0x00000003`) completes a transfer.

| Field | Value |
|---|---|
| `seqnum` | the `seqnum` of the `CMD_SUBMIT` |
| `devid`, `direction`, `ep` | ignored, can be 0 |
| `status` | 0 for success, -32 (`EPIPE`) for a stall, -71 (`EPROTO`) or any other negative number for a transaction error, -104 (`ECONNRESET`) for a cancelled transfer |
| `actual_length` | for IN, the number of bytes returned, at most `transfer_buffer_length`; for OUT, the number of bytes accepted |
| `start_frame`, `error_count` | 0 |
| `number_of_packets` | 0 (all ones is accepted, too) |
| padding | 8 bytes |

For an IN transfer, `actual_length` bytes of data follow the header. For an OUT transfer, no data
follows. Replies may come in any order and are matched by `seqnum`.

A stall (`-32`) halts the endpoint for the guest, which clears it with `CLEAR_FEATURE(ENDPOINT_HALT)`
on endpoint 0 and resets the endpoint; the device sees that request like any other. Control requests
that the device does not support are answered with a stall, too. Other errors are reported to the
guest as a transaction error.

**`RET_UNLINK`** (command `0x00000004`) answers a `CMD_UNLINK`: it has the `seqnum` of the
`CMD_UNLINK`, the `status` -104 if the device cancelled the transfer, and 24 bytes of padding. A
cancelled transfer gets no `RET_SUBMIT`. If the transfer had completed already, the status is 0
and the `RET_SUBMIT` was sent before or is sent right after; the result of a transfer that was
unlinked is dropped. A `RET_UNLINK` for a transfer that no `CMD_UNLINK` named is a framing error.

### Errors that detach the device

`usbvfiod` detaches the device when the device

- sends a `RET_SUBMIT` or `RET_UNLINK` for a `seqnum` that is not in flight (a `RET_SUBMIT` that
  follows a `RET_UNLINK` with -104, or one that arrives twice),
- returns more bytes than `transfer_buffer_length`,
- sends a `CMD_SUBMIT` or `CMD_UNLINK`, a command that does not exist, or a transfer with isochronous
  packets (`number_of_packets` other than 0 and all ones),
- closes the socket or makes it fail.

## Trying it without a guest

`examples/external_hid.rs` is a device server for manual checks. It serves a full speed HID device
(USB 1.10, 8 bytes for endpoint 0, one interrupt IN endpoint with a vendor defined report of two
bytes) on every connection that it accepts:

```console
$ cargo run --example external_hid -- --listen $PWD/D [--close-after SECS]
$ remote --socket H --hub 5 --ports 4 --speed high --vid 067b --pid 2586
$ remote --socket H --external 5.1 --speed full --connect $PWD/D --name hid
$ remote --socket H --tree
5                  high  hub, 4 ports   virtual 1
  5.1              full                 external hid
```

`remote --external` connects to the listening socket of the server and sends the connected socket
with the command 0x15. A real client, such as an emulator that serves several devices, creates a
`socketpair` itself and keeps one end.
