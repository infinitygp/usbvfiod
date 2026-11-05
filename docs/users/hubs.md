# Hubs and Port Paths

usbvfiod can present a tree of USB devices to the guest: virtual USB 2.0 hubs below the root
ports of the controller, with devices on their downstream ports. This reproduces the topology of
hardware that sits behind hubs, such as a device built from several USB devices and hubs, so that a guest
application finds each device at the same place as on the real hardware.

## Port paths

A device is identified by its port path:

| Path | Meaning |
|---|---|
| `5` | the device on root port 5 of the controller |
| `5.1` | port 1 of the hub on root port 5 |
| `5.1.1.4` | port 4 of the hub on port 1 of the hub on port 1 of the hub on root port 5 |

All numbers start at 1. The root port is numbered like the `PORTSC` registers of the controller:
ports 1 to 4 are USB 3 ports and ports 5 to 8 are USB 2 ports. Hubs and everything below them
need a USB 2 root port (5 to 8). A path can go through up to five hubs, and a hub has up to 15
ports. These are the limits of the route string in the slot context of the xHCI specification.

There is one device per path. A guest sees the path as the `LocationPaths` property of the device:
the device `5.1.1.4` on the first controller of the guest is
`PCIROOT(0)#PCI(0305)#PCI(0000)#USBROOT(0)#USB(5)#USB(1)#USB(1)#USB(4)` in Windows.

## Commands

All commands go through the hotplug socket of `usbvfiod` (`--hotplug-socket-path`) with the
`remote` binary. A hub has to exist before a device is attached below it. Detaching a hub detaches
everything below it.

```console
$ remote --socket H --hub 5 --ports 4 --speed high --vid 067b --pid 2586 --product "USB 2.0 Hub"
$ remote --socket H --virtual-hid 5.1 --speed full
$ remote --socket H --attach-at 5.2 --device /dev/bus/usb/005/012
$ remote --socket H --tree
5                  high hub, 4 ports   virtual 1
  5.1              full                virtual 2
  5.2              high                host 005:012
$ remote --socket H --detach-at 5
```

- `--hub PATH` attaches a virtual hub. `--ports` (1 to 15) and `--speed` (`full` or `high`) are
  required. The identity comes from `--vid` and `--pid` (hexadecimal), with `--bcd-device`,
  `--manufacturer`, `--product` and `--serial` as options, or from `--descriptors FILE` (see
  below). A high speed hub has a single transaction translator.
- `--virtual-hid PATH --speed full|high` attaches a test device that enumerates as a HID device
  with a vendor defined report and sends a report four times a second. It needs no hardware.
- `--external PATH --speed low|full|high --connect SOCKET [--name NAME]` attaches an external
  device, a device that another process emulates: `remote` connects to the unix socket `SOCKET` of
  a device server and gives `usbvfiod` the connected socket, over which the server answers the
  transfers of the guest. See [External Devices](./external-devices.md).
- `--attach-at PATH --device /dev/bus/usb/BBB/DDD` attaches a device of the host at a path. Like
  `--attach`, it resets the device and hands the file descriptor to usbvfiod.
- `--detach-at PATH` detaches the device at a path. `--tree` lists all devices with their paths,
  speed and identifier, and `--tree --json` prints the JSON reply.
- `--attach`, `--detach` and `--list` work as before: `--attach` takes the first free root port
  that fits the speed of the device, `--detach` names the device by bus and device number, and
  `--list` shows the devices of the host.

A command that does not work says why: `PathOccupied`, `NoSuchPath` (a root port, hub or hub port
that does not exist), `NotAHub`, `SpeedNotSupported` (a SuperSpeed device on a USB 2 port, or a
high speed device below a full speed hub), `InvalidRequest` (a wrong hub description) or
`FailedToOpenFd`. `remote` exits with an error then.

## The protocol of the hotplug socket

The three-byte commands `attach`, `detach` and `list` and their one-byte replies are unchanged.
The new commands start with a command byte of the range 0x10 to 0x15, followed by the length of
the payload as 32-bit little endian number and the payload as JSON. `attach-at` carries the file
descriptor of the device like `attach`, and the command 0x15 carries one end of a socket. The reply is a 32-bit little endian length and a JSON
object with the `response` (the name of a `Response`), an optional `message` and, for the tree
command, the list `tree`. The code is in `src/hotplug_protocol`.

| Byte | Command | Payload |
|---|---|---|
| 0x10 | attach a host device at a path | `{"path":"5.2","bus":5,"device":12}` and a file descriptor |
| 0x11 | attach a hub | `{"path":"5","spec":{"vid":1659,"pid":9606,"ports":4,"speed":"high"}}` |
| 0x12 | attach the virtual HID | `{"path":"5.1","speed":"full"}` |
| 0x13 | detach at a path | `{"path":"5"}` |
| 0x14 | list the tree | `{}` |
| 0x15 | attach an external device | `{"path":"5.1.1.4","speed":"full","name":"hid"}` and a file descriptor (see [External Devices](./external-devices.md)) |

The `spec` of a hub has the fields `vid`, `pid`, `bcd_device`, `manufacturer`, `product`,
`serial`, `ports`, `speed` and `descriptors` (see `HubSpec`).

## What the hub presents

The hub implements the standard requests of a device and the hub class requests of chapter 11 of
the USB 2.0 specification: `GET_HUB_DESCRIPTOR`, `GET_HUB_STATUS`, `GET_PORT_STATUS`,
`SET_PORT_FEATURE` and `CLEAR_PORT_FEATURE` for power, reset, enable, suspend and the change
bits, and the transaction translator requests `CLEAR_TT_BUFFER`, `RESET_TT`, `GET_TT_STATE` and
`STOP_TT` of a high speed hub. The test mode and the port indicators are not supported and stall.

The status change endpoint answers when a port has a change to report and stays pending
otherwise, like a NAK. A device that connects to a powered port sets the connection and its
change bit. A port reset enables the port in the speed of the device and sets the reset change
bit; it takes no time. A removed device clears connection and enable and sets the connection
change bit.

A hub whose upstream port was suspended by the driver (Windows does this for idle hubs) resumes
the port when a device is plugged in or removed, if the driver armed the remote wakeup of the
hub. The hub above reports it as the end of the suspend of that port, and a hub on a root port
puts the root port in the resume state (`PORTSC.PLS` = 15, `PLC`, port status change event). A
USB 2 root port with a hub on it follows the requests of the driver for the link state U3 and
back to U0. Without this, a device that is attached to an idle tree would not show up in the
guest.

Transfers to the devices below a hub go directly to the device. The controller looks the device
up by the root hub port number and the route string of the slot context, and does not emulate
split transactions. The transaction translator fields of the slot contexts (Hub, MTT, number of
ports, TT think time, TT hub slot and port) are accepted and otherwise ignored. The controller
has 32 device slots, so a tree with nine hubs and several devices fits.

### Descriptors

Without `--descriptors`, the device and configuration descriptors are generated from the options.
A high speed hub is a USB 2.00 device with the protocol "single TT"; a full speed hub is a USB 1.10
device. The configuration has one interface with one interrupt IN endpoint that carries the status
change bitmap. For four ports and high speed, the generated configuration descriptor is identical
to the one that the kernel captured from the Prolific hub `067b:2586`. A high speed hub also
answers the requests for the device qualifier and the other-speed configuration.

With `--descriptors FILE`, the file holds the device descriptor followed by the configuration
descriptor as hexadecimal text, for example the capture of the Prolific hub:

```
12 01 00 02 09 00 01 40 7b 06 86 25 00 00 01 03 00 01 09 02 19 00 01 01 00 e0 32 09 04 00 00 01
09 00 00 00 07 05 81 03 01 00 0c
```

They are served as they are. Strings are not part of a capture of the descriptors; the options
`--manufacturer`, `--product` and `--serial` are served for the string indexes that the device
descriptor names (the Prolific hub names the manufacturer at index 1 and the product at index 3).

The hub class descriptor is always generated from the number of ports. It was **not captured** from
a real hub, but follows the specification: individual port power switching, not a compound
device, over-current reported per port, a transaction translator think time of 8 full speed bit
times, no port indicators, all devices removable, 100 ms from power-on to power good.

## Known limits

- Hubs are USB 2.0 hubs. There are no USB 3 hubs and no SuperSpeed devices below a hub.
- A device below a hub can be full speed or high speed. The hub reports the low speed bit for a
  low speed device of the host, which has not been tried.
- The USB device address in the output slot context is not set, so tools that read it (USBView)
  show address 0.
- A write of `PORTSC.PLS = U3` is ignored for ports of USB 3 and for root ports without a hub.
  The same holds for a downstream port of a hub with a device that is not a hub (a device of the
  host or an external device): `SET_PORT_FEATURE(PORT_SUSPEND)` succeeds, and the port status
  shows the port not suspended, because only a hub can wake a port.
