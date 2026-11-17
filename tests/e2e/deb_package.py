#!/usr/bin/env python3
"""Install the built .deb in a clean 24.04 container, run it, remove it again.

The X11 and GNOME suites run the binary from `target/`, so this layer is the one
that proves the *packaged* artifact: dependencies resolved by apt, the release
build starting, and no leftovers after a purge.

Fails loudly when LXC or the base image is missing: a skipped packaging check
must not look like a passing one.
"""

import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEB = ROOT / "target/deb/sizer_0.1.0_amd64.deb"
IMAGE = "zt-e2e-base"
NAME = f"sizer-gate-deb-{os.getpid()}"

EXTENSION = "gnome-shell/extensions/sizer@sizerlinux.dev"
EXTENSION_SOURCE = ROOT / "platform" / "gnome-extension" / "sizer@sizerlinux.dev"
INSTALLED = [
    "/usr/bin/sizer",
    f"/usr/share/{EXTENSION}/schemas/gschemas.compiled",
    "/usr/share/applications/sizer.desktop",
    "/usr/share/applications/sizer-settings.desktop",
    "/usr/share/icons/hicolor/scalable/apps/sizer.svg",
    "/usr/lib/systemd/user/sizer-daemon.service",
    "/usr/lib/systemd/user/sizer-tray.service",
    "/usr/share/doc/sizer/copyright",
]
# The extension imports its modules by name, so this list is derived from the
# source tree: a module added there is checked here without editing the test.
MODULES = [f"/usr/share/{EXTENSION}/{path.name}"
           for path in sorted(EXTENSION_SOURCE.glob("*.js"))]

failures = []


def report(name, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'}     {name}: {detail}", flush=True)
    if not ok:
        failures.append(name)


def sh(*args, check=True, timeout=600):
    return subprocess.run(args, capture_output=True, text=True, check=check, timeout=timeout)


def in_box(command, timeout=300):
    return subprocess.run(
        ["lxc", "exec", NAME, "--", "sh", "-c", command],
        capture_output=True, text=True, timeout=timeout,
    )


def shipped_dirs(deb):
    """Directories the .deb itself ships, so the shared ones can be told apart."""
    dirs = set()
    for line in sh("dpkg-deb", "-c", str(deb)).stdout.splitlines():
        fields = line.split()
        if len(fields) >= 6 and fields[0].startswith("d") and fields[5].startswith("./"):
            dirs.add(("/" + fields[5][2:]).rstrip("/") or "/")
    return dirs


def main():
    if subprocess.run(["which", "lxc"], capture_output=True).returncode != 0:
        report("lxc_available", False, "lxc is not installed, cannot verify packaging")
        return 1
    aliases = [
        line.split(",")[0].strip()
        for line in sh("lxc", "image", "alias", "list", "--format", "csv", check=False)
        .stdout.splitlines()
        if line.strip()
    ]
    if IMAGE not in aliases:
        report("base_image", False,
               f"image alias {IMAGE} is missing; packaging verification cannot run")
        return 1
    if subprocess.run([str(ROOT / "packaging/build-deb.sh")], cwd=ROOT).returncode != 0:
        report("build_deb", False, "packaging/build-deb.sh failed")
        return 1
    report("build_deb", DEB.exists(), str(DEB))

    sh("lxc", "launch", IMAGE, NAME)
    try:
        deadline = time.time() + 120
        while time.time() < deadline:
            if in_box("true", timeout=30).returncode == 0:
                break
            time.sleep(2)
        else:
            report("container_ready", False, "container did not become usable in 120s")
            return 1
        report("container_ready", True, NAME)

        # Directories that already exist on the image are shared with other
        # packages and stay after a purge; only the ones this package introduced
        # have to disappear. Snapshot them before the install.
        package_dirs = shipped_dirs(DEB)
        shared_dirs = {path for path in package_dirs
                       if in_box(f"test -d {path}").returncode == 0}

        sh("lxc", "file", "push", str(DEB), f"{NAME}/root/sizer.deb")
        # A cold container downloads the whole GTK dependency set; measured at 4m25s
        # on a loaded host, so the install gets its own deadline instead of the
        # short one used for the single queries below.
        # The cloned image can come up with a partial index set: the first refresh
        # then succeeds while a dependency the new package pulls in is still
        # unknown ("not installable"). Refresh once more and retry before failing.
        install_cmd = ("apt-get update -qq && DEBIAN_FRONTEND=noninteractive "
                       "apt-get install -y /root/sizer.deb")
        install = in_box(f"{install_cmd} || {{ {install_cmd}; }}", timeout=900)
        status = in_box("dpkg-query -W -f='${Status}' sizer 2>/dev/null")
        report("apt_install",
               install.returncode == 0 and "install ok installed" in status.stdout,
               status.stdout.strip() or install.stderr[-300:])

        version = in_box("sizer --version")
        report("binary_runs", version.returncode == 0 and "sizer" in version.stdout,
               version.stdout.strip() or version.stderr.strip())
        report("version_license", "MIT License" in version.stdout, version.stdout.strip())
        presets = in_box("XDG_CONFIG_HOME=/root/.config sizer preset list")
        report("release_config_path", presets.returncode == 0, presets.stdout.strip()[:200])
        for path in INSTALLED + MODULES:
            report(f"installed:{Path(path).name}", in_box(f"test -e {path}").returncode == 0, path)

        manifest = in_box("dpkg -L sizer | sed '/^\\.$/d'").stdout.splitlines()
        # `dpkg -L` lists every parent directory up to the root marker; only paths
        # below them are things a purge can remove.
        packaged = [path for path in manifest
                    if path.startswith("/") and path not in ("/", "/.")]
        # Classify while still installed: `dpkg -L` lists files and shared dirs alike.
        packaged_dirs = [path for path in packaged
                         if in_box(f"test -d {path}").returncode == 0]
        packaged_files = [path for path in packaged if path not in packaged_dirs]
        report("manifest", len(packaged_files) > 5,
               f"{len(packaged_files)} files and {len(packaged_dirs)} directories "
               "listed by dpkg -L")

        purge = in_box("DEBIAN_FRONTEND=noninteractive apt-get purge -y sizer 2>&1 | tail -3")
        report("purge", purge.returncode == 0, purge.stdout.strip().splitlines()[-1])
        # The package's own file list drives the residue check, so a file added to
        # the package cannot be forgotten here. Directories the image already had
        # are shared with other packages and are allowed to stay, and so are paths
        # another package owns (installing pulls `Recommends: gnome-shell` in).
        candidates = packaged_files + [path for path in packaged_dirs
                                       if path not in shared_dirs]
        remaining = [path for path in candidates
                     if in_box(f"test -e {path}").returncode == 0]
        owners = in_box("dpkg -S " + " ".join(remaining)).stdout if remaining else ""
        foreign = {line.split(":", 1)[1].strip()
                   for line in owners.splitlines() if ":" in line}
        residue = [path for path in remaining if path not in foreign]
        detail = ", ".join(residue[:6]) if residue else "none"
        if residue:
            detail += " | " + in_box("ls -ld " + " ".join(residue[:3])).stdout.replace("\n", " ; ")
        report("no_residue", not residue, f"{len(residue)} of {len(packaged)} packaged paths "
                                          f"remain: {detail}")
        dangling = in_box("dpkg -s sizer >/dev/null 2>&1")
        report("package_gone", dangling.returncode != 0, "dpkg no longer knows the package")
    finally:
        sh("lxc", "delete", "--force", NAME, check=False)

    print(f"SUMMARY fail={len(failures)}" + (f" ({', '.join(failures)})" if failures else ""),
          flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
