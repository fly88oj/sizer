#!/usr/bin/env bash
# Run the GNOME harness inside the 26.04 VM against a throwaway environment, using
# the extension the deb installed. The harness starts its own headless shell on its
# own session bus, so the running desktop session (which owns the DRM device) is
# untouched; key delivery is proved separately through QMP.
#
# Prerequisites inside the VM: the sizer deb installed, python3-gi, gir1.2-gtk-4.0,
# dbus-x11, glib-compile-schemas. Usage: run_harness_in_vm.sh [harness options]
set -euo pipefail

OUT="${OUT:-/tmp/h50}"
UUID="sizer@sizerlinux.dev"
HERE="${HERE:-/tmp/sizer-e2e}"
EXT_SRC="/usr/share/gnome-shell/extensions/$UUID"

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
    dbus-run-session -- python3 "$HERE/gnome_extension.py" --out "$OUT" \
        --shell-mode ubuntu "$@" || status=$?

exit "$status"
