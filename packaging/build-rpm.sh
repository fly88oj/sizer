#!/usr/bin/env bash
# Build an installable .rpm for Sizer (Fedora / RHEL / openSUSE).
#
# The staged layout comes from `_common.sh`'s sizer_stage_layout; this script
# generates a tiny spec file on the fly and runs rpmbuild on it. The staged
# tree lives in a separate directory so rpmbuild can manage BUILDROOT without
# double-nesting our files.
#
# Usage: packaging/build-rpm.sh [--skip-build] [--target <target.tpl>]
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
sizer_root "$0"
sizer_meta

skip_build=0
target=""
for arg in "$@"; do
    case "$arg" in
        --skip-build) skip_build=1 ;;
        --target)     shift; target="${1:-}" ;;
    esac
done

if [ "$skip_build" -eq 0 ]; then
    cargo build --release --manifest-path "$root/Cargo.toml" -p sizer-cli
fi

sizer_require_binary
sizer_require_extension

rpmbuild_root="$root/target/rpm"
rpmbuild --version >/dev/null 2>&1 || {
    echo "build-rpm: rpmbuild is required (package 'rpm' on Debian/Ubuntu,
         'rpm-build' on Fedora)" >&2
    exit 1
}

# Map Debian-style arch names to rpm's native arch labels.
rpm_arch="$arch"
case "$arch" in
    amd64)   rpm_arch="x86_64" ;;
    arm64)   rpm_arch="aarch64" ;;
    armhf)   rpm_arch="armv7hl" ;;
esac

# Stage into a directory INDEPENDENT of rpmbuild's BUILDROOT — rpmbuild will
# create its own BUILDROOT and our %install section copies from here.
staged="$rpmbuild_root/staged"
rm -rf "$rpmbuild_root"
mkdir -p "$staged"
sizer_stage_layout "$staged"

installed_kb="$(sizer_staged_kb "$staged")"

# Pre-compute the changelog date so heredoc expansion works deterministically.
changelog_date="$(date -u +"%a %b %d %Y")"

spec="$rpmbuild_root/sizer.spec"
cat > "$spec" <<EOF
%define _userunitdir /usr/lib/systemd/user

Name:           $pkg_name
Version:        $version
Release:        1%{?dist}
Summary:        $pkg_summary

License:        $pkg_license
URL:            https://sizerlinux.dev

Requires:       glibc >= $required_glibc
Requires:       libX11
Requires:       libXft
Requires:       fontconfig
Requires:       gtk4 >= 4.6
Suggests:       gnome-shell

%description
$pkg_description

%install
cp -a "$staged"/* %{buildroot}/

%files
%license %{_datadir}/doc/sizer/copyright
%{_bindir}/sizer
%{_datadir}/applications/sizer.desktop
%{_datadir}/applications/sizer-settings.desktop
%{_datadir}/icons/hicolor/scalable/apps/sizer.svg
%{_datadir}/gnome-shell/extensions/sizer@sizerlinux.dev/
/usr/lib/systemd/user/sizer-daemon.service
/usr/lib/systemd/user/sizer-tray.service

%changelog
* $changelog_date Sizer Linux <noreply@sizerlinux.dev> - $version-1
- Initial RPM packaging.
EOF

mkdir -p "$rpmbuild_root/SRPMS" "$rpmbuild_root/RPMS" "$rpmbuild_root/tmp"
rpmbuild \
    --define "_topdir $rpmbuild_root" \
    --target "${target:-$rpm_arch}" \
    -bb "$spec"

# Lift the built RPM out so the Makefile can find it.
built="$(ls "$rpmbuild_root/RPMS/${target:-$rpm_arch}/sizer-${version}-1."*.rpm 2>/dev/null | head -n1)"
echo "built $built"
ls -lh "$built"
