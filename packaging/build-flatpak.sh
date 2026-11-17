#!/usr/bin/env bash
# Build a Flatpak .flatpak bundle for Sizer.
#
# Flatpak is a sandboxed cross-distro package. We use `flatpak-builder` with
# a tiny manifest that installs from the staged tree produced by `_common.sh`.
# The GNOME extension is NOT bundled — a Flatpak-installed app still sees the
# host's GNOME shell and the extension lives in ~/.local/share on the host.
# (The .desktop files we ship still provide the tray + daemon entry points.)
#
# Prerequisites:
#   flatpak install flathub org.freedesktop.Platform//24.08
#   flatpak install flathub org.freedesktop.Sdk//24.08
#
# Usage: packaging/build-flatpak.sh [--skip-build]
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
sizer_root "$0"
sizer_meta

skip_build=0
for arg in "$@"; do
    [ "$arg" = "--skip-build" ] && skip_build=1
done

if [ "$skip_build" -eq 0 ]; then
    cargo build --release --manifest-path "$root/Cargo.toml" -p sizer-cli
fi

sizer_require_binary

if ! command -v flatpak-builder >/dev/null 2>&1; then
    echo "build-flatpak: flatpak-builder not found (install flatpak-builder package)" >&2
    exit 1
fi

target="$root/target/flatpak"
stage="$target/install"
rm -rf "$target"
mkdir -p "$target" "$stage"

# Reuse the common layout minus the GNOME extension (host-owned).
sizer_stage_layout "$stage"
rm -rf "$stage/usr/share/gnome-shell"
# Flatpak expects user units under /app/etc/systemd/user, not /usr/lib/systemd/user.
mkdir -p "$target/wrapper/etc/systemd/user"
cp "$pkg_root/sizer-daemon.service" "$target/wrapper/etc/systemd/user/"
cp "$pkg_root/sizer-tray.service"   "$target/wrapper/etc/systemd/user/"

# Build a local file:// tarball from the staged tree so flatpak-builder can
# ingest it as a "simple" single-source. Use .tar.gz so flatpak-builder auto-
# decompresses it before running build-commands.
tar czf "$target/sizer.tar.gz" -C "$stage" .
sha256sum "$target/sizer.tar.gz" | awk '{print $1}' > "$target/sizer.sha256"

cat > "$target/sizer.yaml" <<'MANIFEST'
app-id: dev.sizerlinux.Sizer
runtime: org.freedesktop.Platform
runtime-version: '24.08'
sdk: org.freedesktop.Sdk
command: sizer
finish-args:
  - --socket=x11
  - --socket=wayland
  - --socket=session-bus
  - --share=ipc
  - --device=all
  - --filesystem=xdg-config
  - --filesystem=xdg-data
  - --filesystem=~/.local/share/applications:rw
  - --filesystem=~/.config/systemd/user:rw
  - --talk-name=org.freedesktop.DBus
  - --own-name=org.kde.StatusNotifierItem.*
modules:
  - name: sizer
    buildsystem: simple
    sources:
      - type: file
        path: sizer.tar.gz
        sha256: REPLACE_ME
    build-commands:
      - tar xzf sizer.tar.gz
      - install -Dm755 usr/bin/sizer -t $FLATPAK_DEST/bin/
      - cp -a usr/share $FLATPAK_DEST/
MANIFEST

# Inject the real sha256 (easy enough to do in sed, no need for template var).
sha="$(cat "$target/sizer.sha256")"
sed -i "s|REPLACE_ME|$sha|" "$target/sizer.yaml"

# Build the .flatpak bundle. --disable-rofiles-fuse is needed for non-root
# builds without FUSE access (common on containerized or locked-down hosts).
flatpak-builder \
    --force-clean \
    --disable-rofiles-fuse \
    --repo="$target/repo" \
    "$target/build" \
    "$target/sizer.yaml"

flatpak build-bundle "$target/repo" "$target/sizer.flatpak" dev.sizerlinux.Sizer || true
echo "built $target/sizer.flatpak (or run flatpak install $target/repo dev.sizerlinux.Sizer locally)"
