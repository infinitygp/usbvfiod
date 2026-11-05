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
  the link state does not change.

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

Only attach host devices that the host can spare: `remote` resets a device, and usbvfiod takes it
from its kernel driver until it is detached.
