#!/usr/bin/env bash
# Build an Arch Linux .pkg.tar.zst for Sizer.
#
# We deliberately avoid makepkg here — it is tightly coupled to Arch Linux
# infrastructure (pacman, archlinux-keyring) and cannot run cleanly on
# Debian/Ubuntu even when installed via apt. Instead we:
#   1. stage the shared layout via _common.sh,
#   2. write a minimal .PKGINFO,
#   3. tar + zstd the whole thing into a .pkg.tar.zst.
#
# Arch pacman consumers accept the resulting file directly — pacman only
# cares that the archive starts with a valid .PKGINFO.
#
# Usage: packaging/build-pacman.sh [--skip-build]
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

build="$root/target/pacman"
rm -rf "$build"
mkdir -p "$build/pkg"
sizer_stage_layout "$build/pkg"

# Arch maps amd64 → x86_64, arm64 → aarch64.
pkg_arch="${arch/amd64/x86_64}"
pkg_arch="${pkg_arch/arm64/aarch64}"
pkg_rel=1

# ---- .PKGINFO ---- pacman reads this as the package metadata.
pkinfo="$build/pkg/.PKGINFO"
{
    echo "pkgname = $pkg_name"
    echo "pkgver = $version-$pkg_rel"
    echo "pkgdesc = $pkg_summary"
    echo "arch = $pkg_arch"
    echo "url = https://sizerlinux.dev"
    echo "license = $pkg_license"
    echo "depends = libc"
    echo "depends = libx11"
    echo "depends = libxft"
    echo "depends = fontconfig"
    echo "depends = gtk4"
    echo "optdepends = gnome-shell: GNOME Shell extension"
    echo "makepkgver = 6.0.2"
    echo "builddate = $(date +%s)"
    echo "packager = Sizer Linux <noreply@sizerlinux.dev>"
    echo "size = $(du -sb "$build/pkg" | cut -f1)"
} > "$pkinfo"

# ---- .BUILDINFO ---- optional but harmless; pacman ignores missing one.
{
    echo "format = 1"
    echo "pkgver = $version-$pkg_rel"
    echo "arch = $pkg_arch"
    echo "pkgbuild_sha256sum = 0000000000000000000000000000000000000000000000000000000000000000"
    echo "builddate = $(date +%s)"
    echo "installed = no"
} > "$build/pkg/.BUILDINFO"

# ---- Pack ---- Arch packages are just .pkg.tar.zst with metadata files.
output="$build/${pkg_name}-${version}-${pkg_rel}-${pkg_arch}.pkg.tar.zst"
rm -f "$output"

# tar needs to be installed (provides --zstd support); zstd compression is
# standard on Arch / Fedora / openSUSE and available via zstd on Debian.
tar -C "$build/pkg" --zstd -cf "$output" \
    .PKGINFO .BUILDINFO usr

echo "built $output"
ls -lh "$output"

# Quick self-check: pacman -Qip reads .PKGINFO from the archive.
if command -v pacman >/dev/null 2>&1; then
    pacman -Qip "$output" 2>/dev/null | head -10 || true
fi
