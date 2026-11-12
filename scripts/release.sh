#!/usr/bin/env bash
# Releases the fork's usbvfiod from this machine: checks, a static musl binary in
# usbvfiod-VERSION-x86_64-linux.tar.gz, a dry-run install into an empty HOME, then the tag VERSION and
# a GitHub release with the package attached. There is no release workflow.
# Usage: scripts/release.sh [--dry-run]   (--dry-run stops before the tag, and only notes a tag
# that already exists for the version)
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
dry=false
[ "${1:-}" = "--dry-run" ] && dry=true
remote=https://github.com/infinitygp/usbvfiod.git
target=x86_64-unknown-linux-musl
out=$HOME/.cache/usbvfiod-release
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$out/target}
# Panic messages carry the source paths of the crates; the home directory of the builder stays out.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~"
git() { command git -c credential.helper= -c 'credential.helper=!gh auth git-credential' "$@"; }
log() { printf '%s release: %s\n' "$(date +%T)" "$*" >&2; }
fail() { log "error: $*"; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag=$version
case $version in *-fork.*) ;; *) fail "version $version is not a fork version (upstream's with -fork.N)" ;; esac
[ -z "$(git status --porcelain)" ] || fail "the working tree has uncommitted changes"
[ "$(git ls-remote "$remote" refs/heads/main | cut -f1)" = "$(git rev-parse HEAD)" ] \
    || fail "HEAD is not what main on GitHub points to; push main first"
# A dry run checks the package of any commit; only a release needs a version that has no tag yet.
taken() { if $dry; then log "note: $1; a release needs a version bump first"; else fail "$1"; fi; }
[ -z "$(git ls-remote "$remote" "refs/tags/$tag")" ] || taken "$tag already exists on GitHub"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then taken "$tag already exists here"; fi
[ ! -d .github/workflows ] || fail ".github/workflows is back (a merge from upstream?): the fork runs no workflow"

log "checks"
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test -q

log "static binary ($target)"
rustup target list --installed | grep -qx "$target" || fail "rustup target add $target"
command -v jq >/dev/null || fail "jq is needed for the licences of the package"
cargo build -q --release --locked --target "$target" --bin usbvfiod
binary=$CARGO_TARGET_DIR/$target/release/usbvfiod
file -b "$binary" | grep -q 'static' || fail "$binary is not statically linked"
! grep -q -a -F "$HOME" "$binary" || fail "$binary contains the path $HOME"

log "package"
name=usbvfiod-$version-x86_64-linux
stage=$out/$name
rm -rf "$stage" && mkdir -p "$stage"
install -m 0755 "$binary" "$stage/usbvfiod"
install -m 0755 scripts/install.sh "$stage/install.sh"
cp LICENSE-APACHE LICENSE-MIT "$stage/"
cargo metadata -q --locked --format-version 1 --filter-platform "$target" \
    | jq -r '.packages[] | [.name, .version, (.license // "see its licence files"), (.manifest_path | rtrimstr("/Cargo.toml"))] | @tsv' \
    > "$out/packages.tsv"
# shortcut: the Rust standard library and musl are named with a link, not with their licence texts;
# add the texts when a package has to stand alone offline.
{
    echo "usbvfiod $version links these crates into its binary. Their licences follow."
    cargo tree -q --locked -e normal --target "$target" -p usbvfiod --prefix none --format '{p}' \
        | sed 's/ (.*//; s/ v/\t/' | sort -u | awk -F'\t' '$1 != "usbvfiod"' \
        | while IFS=$'\t' read -r crate crate_version; do
            IFS=$'\t' read -r _ _ licence dir \
                < <(awk -F'\t' -v n="$crate" -v v="$crate_version" '$1 == n && $2 == v' "$out/packages.tsv")
            printf '\n==== %s %s (%s) ====\n' "$crate" "$crate_version" "$licence"
            found=false
            for f in "$dir"/LICEN[CS]E* "$dir"/COPYING* "$dir"/UNLICENSE; do
                [ -f "$f" ] || continue
                found=true
                printf '\n---- %s ----\n\n' "${f##*/}"
                cat "$f"
            done
            $found || echo "The crate ships no licence file."
        done
    printf '\n==== The Rust standard library (MIT OR Apache-2.0) ====\n\n%s\n' \
        "https://github.com/rust-lang/rust/blob/master/COPYRIGHT"
    printf '\n==== musl libc (MIT) ====\n\n%s\n' "https://git.musl-libc.org/cgit/musl/tree/COPYRIGHT"
} > "$stage/THIRD-PARTY-LICENSES"
printf '%s\n' "$version" > "$stage/VERSION"
archive=$out/$name.tar.gz
tar --owner=0 --group=0 --numeric-owner -C "$out" -czf "$archive" "$name"

log "dry-run install into an empty HOME"
try=$out/try
rm -rf "$try" && mkdir -p "$try/home" "$try/unpacked" "$try/run"
tar -C "$try/unpacked" -xzf "$archive"
in_home() { env -i HOME="$try/home" PATH="$try/home/.local/bin:/usr/bin:/bin" "$@"; }
in_home bash "$try/unpacked/$name/install.sh" >/dev/null
[ "$(in_home usbvfiod --version)" = "usbvfiod $version" ] || fail "installed usbvfiod has the wrong version"
# The server opens its vfio-user and hotplug sockets and ends on SIGTERM. It is started with env
# itself, not through a shell function, so that $! is the server's PID (env execs it).
env -i HOME="$try/home" PATH="$try/home/.local/bin:/usr/bin:/bin" \
    usbvfiod --socket-path "$try/run/S" --hotplug-socket-path "$try/run/H" >"$try/run/log" 2>&1 &
server=$!
for _ in $(seq 50); do [ -S "$try/run/S" ] && [ -S "$try/run/H" ] && break; sleep 0.2; done
[ -S "$try/run/S" ] && [ -S "$try/run/H" ] || { kill "$server" 2>/dev/null; fail "the server opened no sockets: $(tail -3 "$try/run/log")"; }
kill -TERM "$server"
for _ in $(seq 25); do kill -0 "$server" 2>/dev/null || break; sleep 0.2; done
if kill -0 "$server" 2>/dev/null; then kill -KILL "$server"; fail "the server did not end on SIGTERM"; fi
wait "$server" || true
in_home bash "$try/unpacked/$name/install.sh" --uninstall >/dev/null
[ ! -e "$try/home/.local/bin/usbvfiod" ] || fail "uninstall left the binary"
rm -rf "$try" "$stage"

if $dry; then
    log "dry run done: $archive would be released as $tag"
    exit 0
fi

log "tag $tag and release"
git tag -a "$tag" -m "usbvfiod $version"
git push -q "$remote" "refs/tags/$tag"
gh release create "$tag" "$archive" --repo infinitygp/usbvfiod --title "usbvfiod $version" \
    --notes "usbvfiod $version, a fork of cyberus-technology/usbvfiod: a static x86_64 Linux binary. Install: gh release download $tag -R infinitygp/usbvfiod -p 'usbvfiod-*.tar.gz', unpack, run ./install.sh (to ~/.local/bin)."
gh release view "$tag" --repo infinitygp/usbvfiod --json assets --jq '.assets[].name'
