# Vendored crates

## nusb

`nusb/` is nusb 0.2.9 from crates.io with one change, in `src/transfer/internal.rs` and
`src/platform/linux_usbfs/device.rs`: on Linux, a control transfer (`Device::control_in`,
`Device::control_out`, and the same on `Interface`) whose future is dropped before it completed is
cancelled (`USBDEVFS_DISCARDURB`). Upstream leaves it to run until the device answers or its
timeout ends.

usbvfiod needs it to stop a control transfer when the guest stops the default control endpoint,
as an xHCI controller does, instead of ending it with a timeout of its own. Without the change, a
device that never answers a request keeps its default control endpoint busy.

Drop the copy, and the `[patch.crates-io]` entry in `Cargo.toml`, once a nusb release cancels
dropped control transfers.
