#!/usr/bin/env bash
# Shared helpers for all Sizer packaging scripts.
#
# Every format (deb / rpm / AppImage / pacman / flatpak) sources this file
# and calls `sizer_stage_layout` to build a single staged tree that the
# format-specific script then wraps into its final artifact. Keeping the
# layout definition in exactly one place guarantees that no two formats
# drift apart on which files ship.
#
# Usage inside a concrete script:
#   set -euo pipefail
#   source "$(dirname "$0")/_common.sh"
#   sizer_root "$0"           # sets global $root
#   sizer_meta                # sets globals $version $arch $pkg_name ...
#   sizer_stage_layout "$stage_dir"   # populates the staged root tree
#   # ... now build the .deb / .rpm / whatever from $stage_dir
#
# All helper functions below are idempotent: calling `sizer_stage_layout`
# twice on the same directory is fine (it rm -rf's first).

# Resolve the repository root from the caller's path.
sizer_root() {
    local here
    here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    root="$(cd "$here/.." && pwd)"
    export root
}

# Derive package metadata from Cargo.toml + the host distro. Sets:
#   version         e.g. "0.1.0"
#   arch            dpkg --print-architecture or uname -m mapped
#   pkg_name        "sizer"
#   pkg_license     "MIT"
#   pkg_summary     short one-line description
#   pkg_description multi-line description
#   required_glibc  highest GLIBC symbol version in target/release/sizer
#   decl_glibc      "2.39" — the declared minimum that the binary must match
#   extension_src   absolute path to the GNOME extension directory
#   pkg_root        packaging/ directory
sizer_meta() {
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
    pkg_name="sizer"
    pkg_license="MIT"
    pkg_summary="Precise window sizing presets for Linux"
    pkg_description="$(cat <<'EOF'
 Sizer resizes windows to exact pixel or percentage presets and can anchor
 them inside the monitor work area. It ships an X11 command line, a cursor
 menu, a StatusNotifierItem tray and a GNOME Shell extension.
 .
 Enable the GNOME extension once with:
   gnome-extensions enable sizer@sizerlinux.dev
EOF
)"
    decl_glibc="2.39"
    extension_src="$root/platform/gnome-extension/sizer@sizerlinux.dev"
    pkg_root="$root/packaging"

    # Architecture: prefer dpkg --print-architecture (gives x86_64 / aarch64)
    # and fall back to uname -m for distros that don't have dpkg at build time.
    if command -v dpkg >/dev/null 2>&1; then
        arch="${ARCH:-$(dpkg --print-architecture)}"
    else
        local raw
        raw="${ARCH:-$(uname -m)}"
        case "$raw" in
            x86_64)   arch="amd64" ;;
            aarch64)  arch="arm64" ;;
            armv7l)   arch="armhf" ;;
            *)        arch="$raw" ;;
        esac
    fi
    export version arch pkg_name pkg_license pkg_summary pkg_description decl_glibc extension_src pkg_root

    # Compute the highest GLIBC symbol the binary imports. Both build-deb.sh
    # and build-rpm.sh need the same value, so we derive it once here.
    if [ -x "$root/target/release/sizer" ] && command -v objdump >/dev/null 2>&1; then
        required_glibc="$(objdump -T "$root/target/release/sizer" \
            | grep -o 'GLIBC_[0-9.]*' \
            | sed 's/^GLIBC_//' \
            | sort -uV \
            | tail -n1)"
    else
        required_glibc="$decl_glibc"
    fi
    export required_glibc
}

# Assert that the release binary exists and is executable.
sizer_require_binary() {
    if [ ! -x "$root/target/release/sizer" ]; then
        echo "$(basename "$0"): target/release/sizer is missing — run 'make build' first" >&2
        exit 1
    fi
}

# Assert the extension directory exists with every required file.
sizer_require_extension() {
    if [ ! -d "$extension_src" ]; then
        echo "$(basename "$0"): extension directory not found at $extension_src" >&2
        exit 1
    fi
    # extension.js + metadata.json + compat.js + preset_state.js + schemas/*.xml
    for f in metadata.json extension.js compat.js preset_state.js; do
        if [ ! -f "$extension_src/$f" ]; then
            echo "$(basename "$0"): extension is missing $f" >&2
            exit 1
        fi
    done
    if [ ! -d "$extension_src/schemas" ]; then
        echo "$(basename "$0"): extension/schemas/ is missing" >&2
        exit 1
    fi
    if ! command -v glib-compile-schemas >/dev/null 2>&1; then
        echo "$(basename "$0"): glib-compile-schemas is required (libglib2.0-bin / glib2)" >&2
        exit 1
    fi
}

# Populate a staged directory tree with every file every format needs.
#
# Layout (absolute paths relative to $1):
#   usr/bin/sizer
#   usr/share/gnome-shell/extensions/sizer@sizerlinux.dev/... (compiled schemas)
#   usr/share/applications/sizer.desktop
#   usr/share/applications/sizer-settings.desktop
#   usr/share/icons/hicolor/scalable/apps/sizer.svg
#   usr/share/doc/sizer/copyright
#   usr/lib/systemd/user/sizer-daemon.service
#   usr/lib/systemd/user/sizer-tray.service
#
# The caller is responsible for wrapping this tree into the final artifact
# (.deb / .rpm / AppImage / pacman package / flatpak).
sizer_stage_layout() {
    local stage="$1"
    rm -rf "$stage"

    # 1. Binary
    install -Dm755 "$root/target/release/sizer" "$stage/usr/bin/sizer"

    # 2. GNOME extension — schemas compiled at build time so the package
    #    works on machines that don't have glib-compile-schemas installed.
    local ext_target="$stage/usr/share/gnome-shell/extensions/sizer@sizerlinux.dev"
    install -d "$ext_target/schemas"
    install -m644 "$extension_src/metadata.json" "$ext_target/metadata.json"
    install -m644 "$extension_src/extension.js"  "$ext_target/extension.js"
    install -m644 "$extension_src/compat.js"     "$ext_target/compat.js"
    install -m644 "$extension_src/preset_state.js" "$ext_target/preset_state.js"
    install -m644 "$extension_src/schemas/"*.xml "$ext_target/schemas/"
    glib-compile-schemas --strict \
        --targetdir "$ext_target/schemas" "$extension_src/schemas"
    chmod 0644 "$ext_target/schemas/gschemas.compiled"

    # 3. Desktop entries + icon
    install -Dm644 "$pkg_root/sizer.desktop" \
        "$stage/usr/share/applications/sizer.desktop"
    install -Dm644 "$pkg_root/sizer-settings.desktop" \
        "$stage/usr/share/applications/sizer-settings.desktop"
    install -Dm644 "$pkg_root/sizer.svg" \
        "$stage/usr/share/icons/hicolor/scalable/apps/sizer.svg"

    # 4. Copyright — required by .deb policy, useful everywhere else.
    if [ ! -s "$pkg_root/copyright" ]; then
        echo "$(basename "$0"): packaging/copyright is missing" >&2
        exit 1
    fi
    install -Dm644 "$pkg_root/copyright" \
        "$stage/usr/share/doc/sizer/copyright"

    # 5. Systemd user units
    install -Dm644 "$pkg_root/sizer-daemon.service" \
        "$stage/usr/lib/systemd/user/sizer-daemon.service"
    install -Dm644 "$pkg_root/sizer-tray.service" \
        "$stage/usr/lib/systemd/user/sizer-tray.service"
}

# Echo the staged tree size in KB (excluding hidden dirs like DEBIAN / RPM / ...).
sizer_staged_kb() {
    local stage="$1"
    du -sk --exclude='.*' "$stage" 2>/dev/null | cut -f1 || echo 0
}
