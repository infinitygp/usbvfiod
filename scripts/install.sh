#!/usr/bin/env bash
# Installs usbvfiod and remote of this package into ~/.local/bin; `install.sh --uninstall` removes
# them.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$HOME/.local/bin
if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/usbvfiod" "$bin/remote"
    echo "removed $bin/usbvfiod and $bin/remote"
    exit 0
fi
mkdir -p "$bin"
install -m 0755 "$here/usbvfiod" "$here/remote" "$bin/"
echo "installed $("$bin/usbvfiod" --version) as $bin/usbvfiod and $bin/remote"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "note: $bin is not in PATH" ;;
esac
