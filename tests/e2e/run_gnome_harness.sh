#!/usr/bin/env bash
# Isolated GNOME extension run: throwaway HOME/config/data/runtime dirs, private
# session bus, headless GNOME Shell compositor. Nothing touches the user's real
# session.
#
# Usage: tests/e2e/run_gnome_harness.sh [tag] [gnome_extension.py options]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TAG="${1:-gnome46}"
shift || true
OUT="$HERE/artifacts/$TAG"
UUID="sizer@sizerlinux.dev"
EXT_SRC="$HERE/../../platform/gnome-extension/$UUID"

# gvfsd-fuse and the document portal mount inside the private XDG_RUNTIME_DIR.
# A leftover mount makes `rm -rf` fail, so unmount everything below $OUT first.
detach_fuse() {
    local mounts
    mounts="$(awk -v prefix="$OUT/" '$2 ~ "^" prefix {print $2}' /proc/self/mounts 2>/dev/null || true)"
    [ -z "$mounts" ] && return 0
    while read -r mount; do
        [ -z "$mount" ] && continue
        fusermount3 -uz "$mount" 2>/dev/null || fusermount -uz "$mount" 2>/dev/null || umount -l "$mount" 2>/dev/null || true
    done <<< "$mounts"
}

detach_fuse
rm -rf "$OUT"
mkdir -p "$OUT"/run "$OUT"/home "$OUT"/config "$OUT"/data "$OUT"/cache "$OUT"/tmp
chmod 700 "$OUT"/run

EXT_DST="$OUT/data/gnome-shell/extensions/$UUID"
mkdir -p "$(dirname "$EXT_DST")"
cp -r "$EXT_SRC" "$EXT_DST"
glib-compile-schemas "$EXT_DST/schemas"

export XDG_DATA_DIRS="/usr/local/share:/usr/share"
env GSETTINGS_BACKEND=keyfile XDG_CONFIG_HOME="$OUT/config" XDG_DATA_DIRS="$XDG_DATA_DIRS" \
    gsettings set org.gnome.shell enabled-extensions "['$UUID']"
env GSETTINGS_BACKEND=keyfile XDG_CONFIG_HOME="$OUT/config" XDG_DATA_DIRS="$XDG_DATA_DIRS" \
    gsettings set org.gnome.shell disable-user-extensions false

status=0
env -i \
    PATH="/usr/local/bin:/usr/bin:/bin" \
    HOME="$OUT/home" \
    XDG_RUNTIME_DIR="$OUT/run" \
    XDG_CONFIG_HOME="$OUT/config" \
    XDG_DATA_HOME="$OUT/data" \
    XDG_DATA_DIRS="$XDG_DATA_DIRS" \
    XDG_CACHE_HOME="$OUT/cache" \
    GSETTINGS_BACKEND=keyfile \
    TMPDIR="$OUT/tmp" \
    LANG=C.UTF-8 \
    dbus-run-session -- python3 "$HERE/gnome_extension.py" --out "$OUT" "$@" || status=$?

detach_fuse
exit "$status"
