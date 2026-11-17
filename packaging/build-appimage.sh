#!/usr/bin/env bash
# Build an AppImage for Sizer — single-file cross-distro binary.
#
# AppImages run on any modern Linux distro without installation. This script
# uses the official `appimagetool` to package the staged tree produced by
# `_common.sh` plus a runtime-captured ld-linux + shared libs needed by
# sizer itself.
#
# Usage: packaging/build-appimage.sh [--skip-build]
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
sizer_require_extension

# --- Stage content into an AppDir ---
target="$root/target/appimage"
appdir="$target/sizer.AppDir"
output="$target/sizer-${version}-${arch}.AppImage"
rm -rf "$target"
mkdir -p "$target" "$appdir"

# AppImage internal layout differs from distro packages — binary goes into
# AppDir/usr/bin, desktop/icon stay where the autostart expects them.
install -Dm755 "$root/target/release/sizer" "$appdir/usr/bin/sizer"
install -Dm644 "$pkg_root/sizer.desktop" "$appdir/sizer.desktop"
install -Dm644 "$pkg_root/sizer.svg" "$appdir/sizer.svg"
install -Dm644 "$pkg_root/sizer.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/sizer.svg"

# AppImages want a PNG at the top level (sizer.png) so appimagetool can
# auto-create .DirIcon. Convert the SVG with ImageMagick if available;
# otherwise fall back to copying the SVG (appimagetool still warns but works).
if command -v convert >/dev/null 2>&1; then
    convert -background none -resize 256x256 "$pkg_root/sizer.svg" "$appdir/sizer.png" 2>/dev/null || \
        cp "$pkg_root/sizer.svg" "$appdir/sizer.png"
else
    cp "$pkg_root/sizer.svg" "$appdir/sizer.png"
fi

# GNOME extension ships under /usr/share so the packaged app is self-contained
# when the user copies or extracts the AppImage.
ext_target="$appdir/usr/share/gnome-shell/extensions/sizer@sizerlinux.dev"
install -d "$ext_target/schemas"
install -m644 "$extension_src/metadata.json" "$ext_target/metadata.json"
install -m644 "$extension_src/extension.js"  "$ext_target/extension.js"
install -m644 "$extension_src/compat.js"     "$ext_target/compat.js"
install -m644 "$extension_src/preset_state.js" "$ext_target/preset_state.js"
install -m644 "$extension_src/schemas/"*.xml "$ext_target/schemas/"
glib-compile-schemas --strict --targetdir "$ext_target/schemas" "$extension_src/schemas"
chmod 0644 "$ext_target/schemas/gschemas.compiled"

install -Dm644 "$pkg_root/copyright" "$appdir/usr/share/doc/sizer/copyright"

# --- Runtime probe — copy any shared libs sizer wants that aren't on this host ---
# The binary loads libc6, libX11, libXft, fontconfig, GTK4 etc. Most of these
# are present on every modern distro, but bundling them makes the AppImage
# portable across distros with older (or newer) sonames. Leave libc itself
# alone — bundling glibc almost always breaks; the host provides it.
mkdir -p "$appdir/usr/lib"
skip_libc=1
ldd "$root/target/release/sizer" | awk '{print $3}' | while read -r lib; do
    [ -z "$lib" ] && continue
    # Skip libc + libm + ld-linux — distro-provided, never bundled.
    case "$(basename "$lib")" in
        libc.so*|libm.so*|ld-linux*|libpthread.so*|libdl.so*|librt.so*)
            continue ;;
    esac
    if [ -f "$lib" ]; then
        cp -n "$lib" "$appdir/usr/lib/" || true
    fi
done

# --- AppRun — AppImages need one of these at the root ---
cat > "$appdir/AppRun" <<'APPRUN_EOF'
#!/usr/bin/env bash
# Re-exec the real binary with bundled libs on LD_LIBRARY_PATH.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/usr/lib:${LD_LIBRARY_PATH:-}"
exec "$here/usr/bin/sizer" "$@"
APPRUN_EOF
chmod 755 "$appdir/AppRun"

# --- Finalize ---
if ! command -v appimagetool >/dev/null 2>&1; then
    echo "build-appimage: appimagetool not found." >&2
    echo "  Download the official binary:" >&2
    echo "    wget -q https://github.com/AppImage/appimagetool/releases/latest/download/appimagetool-$(uname -m).AppImage -O /usr/local/bin/appimagetool && chmod +x /usr/local/bin/appimagetool" >&2
    exit 1
fi

appimagetool "$appdir" "$output"
echo "built $output"
ls -lh "$output"
