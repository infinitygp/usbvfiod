# Working rules for this fork

This is [infinitygp/usbvfiod](https://github.com/infinitygp/usbvfiod), a fork of
[cyberus-technology/usbvfiod](https://github.com/cyberus-technology/usbvfiod): a vfio-user server
that gives a **QEMU** guest (`vfio-user-pci` client), in particular a **Windows 11 guest**, an xHCI
controller. The controller has virtual USB 2.0 hubs below its root ports, and devices attach at
port paths behind them: devices of the host, a virtual HID test device, and external devices that
another process serves over a unix socket.

- Work happens on `main`, which holds upstream's code and the fork's. `origin` is the fork (push
  there), `upstream` is Cyberus. Never push to `upstream`. Merge `upstream/main` into `main` to follow
  upstream. GitHub Actions are off in the fork and it publishes no crate: a merge that brings back
  `.github/workflows/` drops them again.
- Keep the upstream code style and the guidelines in `docs/developers/guidelines.md`. Commit
  messages are one line in the style of upstream: `module: what changed`.
- Everything in the repository is in English. The repository is public: name no private projects,
  hosts, tools, tasks or people in code, docs or commit messages.

## Build and test

```console
$ cargo build
$ cargo fmt --all --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
```

The checks after `cargo build` must pass without warnings before a commit. They are what upstream's `flake.nix`
runs (`nix flake check` also runs nextest, rustdoc, the dependency audits, formatters for Nix and
TOML and the NixOS VM tests under `nix/checks`, which need Cloud Hypervisor). The unit tests need
no device, no VM and no network.

## The Windows check

`docs/developers/windows-guests.md` has the procedure and the diagnostics. In short:

1. Run a Windows 11 guest whose RAM is a shared memfd (`docs/users/qemu.md`), for example as a
   libvirt domain of the user session.
2. Start `usbvfiod -vv --no-color --reconnect --socket-path S --hotplug-socket-path H --pcap-path P
   [--device /dev/bus/usb/BBB/DDD]` in a scratch directory.
3. Hot-add the controller with `virsh -c qemu:///session qemu-monitor-command DOMAIN
   '{"execute":"device_add","arguments":{"driver":"vfio-user-pci","id":"usbvfiod0","bus":"pci.14","socket":{"type":"unix","path":"…/S"}}}'`
   (`bus` is a free PCIe root port of the domain).
4. Read the state in the guest: `Get-PnpDevice`, `DEVPKEY_Device_ProblemCode`,
   `DEVPKEY_Device_LocationPaths`, the System event log. The expected result is "Standard USB 3.20
   eXtensible Host Controller - 1.0 (Microsoft)" and "USB Root Hub (USB 3.0)", both with status OK
   and problem code 0.

No hardware at hand, or no device to risk? The ignored test `serve_a_virtual_hid_to_a_guest`
(`src/xhci_backend.rs`, the device is in `src/device/xhci/virtual_hid.rs`) serves a virtual full speed HID device with a vendor defined
report that sends 4 reports per second, instead of `usbvfiod`:

```console
$ cargo test --bin usbvfiod --no-run   # prints the test executable
$ USBVFIOD_TEST_SOCKET=$PWD/V [USBVFIOD_TEST_FLAP_SECS=8] \
    target/debug/deps/usbvfiod-HASH serve_a_virtual_hid_to_a_guest --ignored --nocapture
```

Add it with `device_add` as in step 3 (use another `id` and `bus` when the guest has a controller of
`usbvfiod` already). With `USBVFIOD_TEST_FLAP_SECS=N` the device is detached and attached every N
seconds. Windows lists it as `USB\VID_1209&PID_DE00\0001`.

## Release

The version is upstream's with the suffix `-fork.N` (`0.4.0-fork.1`), so `usbvfiod --version` tells
the fork from upstream. The package is `usbvfiod-VERSION-x86_64-linux.tar.gz`: static musl binaries of
`usbvfiod` and `remote`, which run on any x86_64 Linux, `install.sh`, the licences of usbvfiod and `THIRD-PARTY-LICENSES`
with those of the crates in the binary. Others download the newest release by the repository name
and the pattern `usbvfiod-*.tar.gz`, so keep both, and publish a release as the latest, not as a
prerelease. Releases are made locally and run the whole chain without stopping between steps:

1. Bump `version` in `Cargo.toml` (the next `-fork.N`, or the new upstream version with `-fork.1`
   after a merge), run `cargo build` so that `Cargo.lock` follows, commit `release: usbvfiod VERSION`
   and push `main`.
2. Run `scripts/release.sh --dry-run`. It refuses a dirty tree or an unpushed `main`, notes an
   existing tag (the release refuses it), runs the checks under "Build and test", builds the static
   binary without the paths of the home directory, packs it with the licences, installs it into an
   empty `HOME`, checks `usbvfiod --version` and that the server opens its sockets, and uninstalls
   it. It needs `rustup target add x86_64-unknown-linux-musl`, `file`, `jq` and `gh`.
3. Run `scripts/release.sh`. It repeats step 2, then tags `VERSION` (no `v` prefix), pushes the tag
   and creates the GitHub release with the package attached.
4. Install it: `gh release download VERSION -R infinitygp/usbvfiod -p 'usbvfiod-*.tar.gz'`, unpack,
   and run `./install.sh` (both binaries into `~/.local/bin`).

## Hubs and port paths

The controller holds a tree of devices (`src/device/xhci/device_tree.rs`). A device has a port path:
`R` for root port R (USB 2 root ports are 5 to 8) and `R.p1.p2` for port p2 of the hub on port p1 of
the hub on root port R, up to five hubs deep (`docs/users/hubs.md`). The hotplug socket takes them
with `remote`:

```console
$ remote --socket H --hub 5 --ports 4 --speed high --descriptors hub.hex --product "USB 2.0 Hub"   # hex text, docs/users/hubs.md
$ remote --socket H --virtual-hid 5.1.4 --speed full
$ remote --socket H --attach-at 5.1.4 --device /dev/bus/usb/BBB/DDD   # a host device, see below
$ remote --socket H --external 5.1.4 --speed full --connect D --name N   # a device server, docs/users/external-devices.md
$ remote --socket H --tree
$ remote --socket H --detach-at 5.1
```

The old `--attach`, `--detach` and `--list` still work (`--attach` takes the first free root port of
the speed). A device is found by the root hub port number and route string of its slot context.
There are 32 device slots.

A hardware-free check of a tree: hubs at 5 and 5.1, both high speed with 4 ports, the virtual HID at
5.1.4 (full speed) and 5.2 (high speed). Windows lists the devices with the location paths
`...#USBROOT(0)#USB(5)#USB(1)#USB(4)` and `...#USBROOT(0)#USB(5)#USB(2)`, and
`docs/developers/usbtree.ps1` shows the speeds as Windows reports them. Detach and attach a device,
a hub subtree and the top hub again: `Present` goes False and True again.

Things to know:

- QEMU 11.1.3 aborts the whole VM when the server dies (`error_setv: Assertion '*errp == NULL'`) and
  when the device is removed with `device_del` (a `memory_region_del_subregion` assertion in the
  MSI-X teardown). That is not a bug of this repository; start a fresh guest instead of
  hot-removing. `pkill -f` on the usbvfiod command line also matches the shell that runs it: kill by
  PID.
- QEMU's own `qemu-xhci` controller is accepted by the same Windows driver. Its source
  (`hw/usb/hcd-xhci*.c`) is the reference when Windows rejects something of ours.
- The check of the unit tests does not replace the check in the guest, and the other way round.

## Devices of the host

- `remote --attach` and `--attach-at` reset a device of the host and hand it to usbvfiod, which takes
  it from its kernel driver. Only use a device that the task names and that the host can spare,
  never the keyboard, mouse or other devices the host needs.
- Find a device by its sysfs path right before use (`/sys/bus/usb/devices/<port>`), verify
  `idVendor:idProduct` there and compute `/dev/bus/usb/BBB/DDD` from `busnum` and `devnum`. Never
  pass a node by guessing, never iterate over `/dev/bus/usb`, never match devices by VID:PID across
  the bus.
- usbvfiod only forwards what the guest sends. Do not send any vendor-specific request or report
  yourself, and never send firmware, flash or reset-to-factory commands. Check the pcap of the run
  when in doubt.
- When done with a device, stop usbvfiod and check that
  `/sys/bus/usb/devices/<port>/<port>:1.0/driver` is the kernel driver again (`usbhid` for a HID
  device). If it is not, report it. Do not try to rebind with root.
