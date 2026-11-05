# Using usbvfiod with QEMU

QEMU connects to usbvfiod with its `vfio-user-pci` device (QEMU 10.1 and later). Version 11.1.3
is tested, with a Windows 11 guest.

## Shared guest memory

usbvfiod reads and writes the rings and buffers of the guest driver directly in the memory of the
guest. QEMU hands the memory to usbvfiod as file descriptors, so the guest RAM has to be a shared
memory backend:

```console
$ qemu-system-x86_64 \
    -machine q35,memory-backend=ram \
    -object memory-backend-memfd,id=ram,size=4G,share=on \
    ...
```

With libvirt, use `<memoryBacking><source type='memfd'/><access mode='shared'/></memoryBacking>`.

QEMU also maps memory that has no file descriptor (ROMs, the VGA window, the video memory of a
display adapter). usbvfiod keeps track of these ranges, but it cannot access them. A DMA access to
one of them reads all bits set and writes are dropped, and usbvfiod logs a warning for the first
access to each range. A guest driver does not put its DMA buffers there as long as the RAM is shared.

## Attaching the controller

Start usbvfiod first. The `--reconnect` option keeps it running after the guest disconnected, so
that the next QEMU can attach to the same socket:

```console
$ usbvfiod --socket-path /path/usbvfiod.sock --hotplug-socket-path /path/usb-hotplug.sock --reconnect
```

Add the controller on the command line:

```console
$ qemu-system-x86_64 ... \
    -device '{"driver":"vfio-user-pci","socket":{"type":"unix","path":"/path/usbvfiod.sock"}}'
```

or to a running machine, with a free PCIe root port as `bus`:

```console
$ virsh qemu-monitor-command DOMAIN '{"execute":"device_add","arguments":{"driver":"vfio-user-pci","id":"usbvfiod0","bus":"pci.14","socket":{"type":"unix","path":"/path/usbvfiod.sock"}}}'
```

Windows 11 and Linux guests load their built-in xHCI driver for the controller. Devices are
attached and detached with the `remote` binary as described in [Basic Setup](./basic.md).

## Known limitations

- Removing the device from a running QEMU 11.1.3 (`device_del`) aborts QEMU in the teardown of the
  MSI-X pending bit array region (`memory_region_del_subregion` assertion). This happens in QEMU's
  `vfio-user-pci` and is independent of the server.
- If usbvfiod exits while QEMU is connected, QEMU aborts with `error_setv: Assertion '*errp == NULL'
  failed` and takes the guest down.
- The server answers the memory map requests of QEMU with the file descriptors that QEMU sent, so
  the guest memory must stay mapped in QEMU's memfd. Memory hot-plug is not tested.
