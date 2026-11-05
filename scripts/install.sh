#!/usr/bin/env bash
# Installs the usbvfiod of this package into ~/.local/bin; `install.sh --uninstall` removes it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$HOME/.local/bin
if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/usbvfiod"
    echo "removed $bin/usbvfiod"
    exit 0
fi
mkdir -p "$bin"
install -m 0755 "$here/usbvfiod" "$bin/usbvfiod"
echo "installed $("$bin/usbvfiod" --version) as $bin/usbvfiod"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "note: $bin is not in PATH" ;;
esac
