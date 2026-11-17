#!/usr/bin/env bash
# Build an installable .deb for Sizer without debhelper.
#
# The staged layout comes from `_common.sh`'s sizer_stage_layout; this script
# only adds the DEBIAN/ control + postinst + triggers and wraps it with
# dpkg-deb. This ensures the .deb ships exactly the same files as the rpm
# / AppImage / pacman builds.
#
# Usage: packaging/build-deb.sh [--skip-build]
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

stage="$root/target/deb/sizer_${version}_${arch}"
output="$root/target/deb/sizer_${version}_${arch}.deb"

# Declared minimum GLIBC that the binary must actually require. The build
# refuses silently bumping to a newer toolchain that would ship an unusable
# package on older distros.
if [ -z "$required_glibc" ] || [ "$required_glibc" != "$decl_glibc" ]; then
    echo "build-deb: binary needs libc6 >= ${required_glibc:-?}," \
        "package declares $decl_glibc — bump decl_glibc or rebuild on glibc $decl_glibc" >&2
    exit 1
fi

sizer_stage_layout "$stage"

install -d "$stage/DEBIAN"
install -m755 "$pkg_root/postinst" "$stage/DEBIAN/postinst"
install -m755 "$pkg_root/postrm"   "$stage/DEBIAN/postrm"
install -m644 "$pkg_root/triggers" "$stage/DEBIAN/triggers"
installed_kb="$(sizer_staged_kb "$stage")"

cat > "$stage/DEBIAN/control" <<EOF
Package: sizer
Version: $version
Architecture: $arch
Maintainer: Sizer Linux <noreply@sizerlinux.dev>
Installed-Size: $installed_kb
Section: utils
Priority: optional
Depends: libc6 (>= $required_glibc), libx11-6, libxft2, libfontconfig1, libgtk-4-1 (>= 4.6)
Recommends: gnome-shell
Description: $pkg_summary
$pkg_description
EOF

rm -f "$output"
dpkg-deb --root-owner-group --build "$stage" "$output"
echo "built $output"
dpkg-deb --info "$output" | sed -n '1,12p'
