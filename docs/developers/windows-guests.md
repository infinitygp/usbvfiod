# Windows Guests

Windows 11 uses its inbox xHCI driver (`usbxhci.sys`) for the controller. The root hub and
devices behind it enumerate without an additional driver.

## What the driver needs

Most of this holds for every guest, but Windows is the first driver that exercises it:

- It resets the controller at every start (`USBCMD.HCRST`). To stop the controller it clears
  `USBCMD.RS` while the command ring has run, and then writes zero to `CRCR`, `DCBAAP` and the event
  ring registers. `CRCR.CRR` has to go back to zero when `USBCMD.RS` is cleared, and the write to
  `CRCR` has to be accepted.
- It reads and writes the MMIO registers with 32-bit accesses. The client may split or merge
  accesses, so every access size up to 8 bytes is served.
- After a Host Controller Reset the port registers start from scratch and a connected device
  appears as newly connected, with `USBSTS.PCD` set. Windows enumerates a device that was attached
  before the driver started, and one that stays attached over a guest reboot, from the port
  registers.
- `USBSTS.EINT` and `IMAN.IP` are set when an event is put on the event ring and are cleared by
  writing 1. `USBSTS.PCD` is set for every port status change event, including one that was
  generated before the driver configured the event ring. The interrupt is only raised while
  `IMAN.IE` is set.
- A full event ring is reported with an Event Ring Full Error event in the last free slot. The
  events that did not fit wait until the driver advanced the dequeue pointer.
- During a stop it writes `PORTSC.PLS = U3` with `LWS` set to the ports. The write is ignored and
  the link state does not change, except for a USB 2 port with a hub on it (see below).
- A Stop Endpoint command on an endpoint with a pending transfer is answered by a Transfer Event
  with the completion code Stopped, before the completion of the command. Without it, Windows
  waits 2 seconds before it goes on. It stops the status change endpoint of an idle hub this way
  when it suspends the hub. The controller collects all TRBs of a TD before it talks to the device,
  so a stop abandons the whole TD: the event names the first TRB of the TD with its full length
  as the residual (the length is not valid for a TRB that moves no data), the dequeue pointer of
  the endpoint context points to that TRB, and the TD starts over when the endpoint continues.
- Windows suspends hubs that have no active device, and relies on the remote wakeup of the hub to
  learn about a device that is plugged in later (see [Hubs and Port Paths](../users/hubs.md)). When
  the whole tree is idle, it suspends the root port of the top hub with `PORTSC.PLS = U3`. A USB 2 root
  port with a hub on it goes to U3 on this request. When a device comes or goes below the hub, the
  port goes to the resume state (`PLS = 15`, `PLC` set, port status change event), and the driver
  ends the resume with `PLS = U0`. `PLC` is a write-1-to-clear bit.
- It needs the USB 2 hub class requests of chapter 11 to answer in the states that the
  specification describes, and shows a hub with the speed of its device descriptor in USBView. A
  high speed hub below a full speed hub does not work, so the controller refuses it.

## Requests of QEMU's vfio-user client

QEMU 11.1.3 sends these requests, all of which usbvfiod handles:

- `DMA_MAP` for the RAM with the memfd and for ROM and device memory without a file descriptor.
  A region of RAM is split in several maps around the legacy VGA window and the ACPI tables.
- `DMA_UNMAP` for a range that covers several mappings, a part of a mapping, or nothing mapped.
  QEMU sends them whenever its memory map changes, and in large numbers at a guest reboot.
- `DEVICE_RESET` at guest reset and when the device is added.
- `SET_IRQS` for the MSI-X vectors: a file descriptor to enable, none to disable, and a trigger
  action with no data for all vectors. QEMU keeps the MSI-X table itself and only reads the
  pending bit array of the controller.
- `REGION_READ` and `REGION_WRITE` for config space and the MMIO BAR, in posted and in
  confirmed form.

## Running the check

Start a Windows 11 guest whose RAM is a shared memfd (see [Using usbvfiod with
QEMU](../users/qemu.md)), here the libvirt domain `DOMAIN` of the user session:

```console
$ mkdir -p ~/.cache/usbvfiod-dev && cd ~/.cache/usbvfiod-dev
$ usbvfiod -vv --no-color --reconnect --socket-path $PWD/S --hotplug-socket-path $PWD/H --pcap-path $PWD/P &
$ virsh -c qemu:///session qemu-monitor-command DOMAIN '{"execute":"device_add","arguments":{"driver":"vfio-user-pci","id":"usbvfiod0","bus":"pci.14","socket":{"type":"unix","path":"'$PWD'/S"}}}'
```

Then ask Windows, in a PowerShell of the guest:

```console
PS> Get-PnpDevice | Where-Object { $_.InstanceId -like 'USB\*' -or $_.InstanceId -like 'PCI\VEN_1B36&DEV_000D*' } | ForEach-Object { $p = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_ProblemCode).Data; $loc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_LocationPaths -ErrorAction SilentlyContinue).Data -join ' | '; '{0} | {1} | problem={2} | {3} | {4}' -f $_.FriendlyName, $_.Status, $p, $_.InstanceId, $loc }
```

The controller is "Standard USB 3.20 eXtensible Host Controller - 1.0 (Microsoft)" with status OK and
problem code 0, and its root hub "USB Root Hub (USB 3.0)" has status OK. The guest also has the
controller of QEMU itself, `qemu-xhci`, which the same driver accepts: compare against it when
something is wrong. Its source is in `hw/usb/hcd-xhci*.c` of QEMU.

More diagnostics:

- `Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='USBXHCI','USBHUB3','Microsoft-Windows-Kernel-PnP'}`
  and the log `Microsoft-Windows-USB-USBXHCI-Operational`.
- The `--pcap-path` file of usbvfiod shows the USB traffic that the guest sends to a device.
- QMP `trace-event-set-state` with `vfio_user*` shows the messages in QEMU's log
  (`~/.cache/libvirt/qemu/log/DOMAIN.log` for a domain of the user session).
- Attach a device with `remote --socket H --attach /dev/bus/usb/BBB/DDD` and detach it with
  `remote --socket H --detach BBB DDD`.

The topology as Windows reports it, with the speed of every hub and device, comes from
[`usbtree.ps1`](./usbtree.ps1), which walks the root hubs with the IOCTLs that USBView uses. Copy
it into the guest and run it there:

```console
PS> powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\usbtree.ps1
```

Without hardware, the hubs and devices of a tree can come from the virtual hub and the virtual HID
device (see [Hubs and Port Paths](../users/hubs.md)): attach the hubs and devices with `remote`
before or after the controller is added to the guest, then compare `Get-PnpDevice`
(`DEVPKEY_Device_LocationPaths`, status, problem code) and `usbtree.ps1` with the tree that
`remote --tree` shows. Detach and attach devices and whole hubs at their paths to check that Windows
follows (`Present` is False, then True again, with status OK).

An external device (see [External Devices](../users/external-devices.md)) needs no hardware either.
`examples/external_hid.rs` serves a full speed HID with USB 1.10 and 8 bytes for endpoint 0 on a
unix socket; `remote --external` below the hubs of a tree attaches it:

```console
$ cargo run --example external_hid -- --listen $PWD/D &
$ remote --socket H --external 5.1.1.4 --speed full --connect $PWD/D --name hid
```

Windows lists it as `USB\VID_1209&PID_DE01\0001` with the location path
`...#USBROOT(0)#USB(5)#USB(1)#USB(1)#USB(4)`, `usbtree.ps1` shows `speed=full` and `bcdUSB=0110`,
and the HID driver polls the interrupt endpoint from the start. Disabling the device in the guest
(`Disable-PnpDevice`) makes the controller send a `CMD_UNLINK` for the waiting transfer. Stopping
the server closes the connection, which detaches the device (`Present` is False), and a new server
and a new `remote --external` bring it back.

Only attach host devices that the host can spare: `remote` resets a device, and usbvfiod takes it
from its kernel driver until it is detached.
