#!/usr/bin/env python3
"""Prove that `sizer preset ...` reaches a running `sizer daemon`.

A fake `org.sizerlinux.Shell` service is exposed on a private session bus.
It records every `SetPresets` payload and answers `MenuState` the way the
real extension does (depth-first preset labels), so the daemon's "did the
shell lose the presets?" check behaves normally.

The daemon watches the configuration file. The CLI writes it with an atomic
temp+rename, which keeps the same second and often the same size, so this
script also proves the inode part of the file stamp.

Run: python3 tests/e2e/preset_daemon.py --sizer target/debug/sizer
"""

import argparse
import json
import os
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

BUS_NAME = "org.sizerlinux.Shell"
OBJECT_PATH = "/org/sizerlinux/Shell"
INTERFACE = "org.sizerlinux.Shell"


def flatten_labels(presets):
    """Mirror crates/config/src/menu.rs: depth-first, groups by first appearance.

    Separators occupy a row but are never activatable, so they have no label.
    """
    nodes = []
    for preset in presets:
        if preset.get("kind") == "separator":
            continue
        path = [part.strip() for part in (preset.get("group") or "").split("/") if part.strip()]
        level = nodes
        for name in path:
            group = next((node for node in level if node[0] == "group" and node[1] == name), None)
            if group is None:
                group = ["group", name, []]
                level.append(group)
            level = group[2]
        level.append(["preset", preset.get("label", "")])

    labels = []

    def walk(items):
        for item in items:
            if item[0] == "preset":
                labels.append(item[1])
            else:
                walk(item[2])

    walk(nodes)
    return labels


def preset_entries(payload):
    """Preset list of a `SetPresets` payload (version 2 object or old array)."""
    data = json.loads(payload)
    return data.get("presets", []) if isinstance(data, dict) else data


class FakeShell:
    def __init__(self, connection):
        self.connection = connection
        self.payloads = []
        self.menu_labels = []
        xml = f"""
        <node><interface name="{INTERFACE}">
          <method name="SetPresets"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
          <method name="MenuState"><arg type="s" direction="out"/></method>
          <method name="ActivateMenuRow"><arg type="u" direction="in"/></method>
          <method name="LastResult"><arg type="s" direction="out"/></method>
          <method name="CloseMenu"/>
          <method name="KeybindingState"><arg type="s" direction="out"/></method>
        </interface></node>
        """
        info = Gio.DBusNodeInfo.new_for_xml(xml)
        connection.register_object(OBJECT_PATH, info.interfaces[0], self.on_call, None, None)

    @staticmethod
    def judge(entries):
        """Keep valid entries and skip labels starting with `skip-`, like the
        real extension does with entries it cannot honour."""
        kept, skipped = [], []
        for entry in entries:
            label = str(entry.get("label", "")) if isinstance(entry, dict) else ""
            if label.startswith("skip-"):
                skipped.append({"label": label, "reason": "test skip"})
            else:
                kept.append(entry)
        return kept, skipped

    def on_call(self, _conn, _sender, _path, _iface, method, parameters, invocation):
        if method == "SetPresets":
            payload = parameters.unpack()[0]
            self.payloads.append(payload)
            entries = preset_entries(payload)
            kept, skipped = self.judge(entries)
            self.menu_labels = flatten_labels(kept)
            reply = {"ok": True, "presets": len(kept), "skipped": skipped}
            invocation.return_value(GLib.Variant("(s)", (json.dumps(reply),)))
        elif method == "MenuState":
            state = {"presets": self.menu_labels, "open": False}
            invocation.return_value(GLib.Variant("(s)", (json.dumps(state),)))
        elif method == "LastResult":
            invocation.return_value(GLib.Variant("(s)", ('{"selected": null}',)))
        elif method == "KeybindingState":
            invocation.return_value(GLib.Variant("(s)", ('{"registered": true}',)))
        else:
            invocation.return_value(None)


def pump(seconds, until=None):
    context = GLib.MainContext.default()
    deadline = time.time() + seconds
    while time.time() < deadline:
        while context.pending():
            context.iteration(False)
        if until is not None and until():
            return True
        time.sleep(0.01)
    return until is None


def drain_output(proc):
    """Bytes the daemon printed so far, without blocking.

    Reads the raw fd after select: readline() would pull several lines into the
    TextIOWrapper buffer, leaving select with nothing to report and the caller
    with only the first line.
    """
    chunks = []
    fd = proc.stdout.fileno()
    while True:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if not ready:
            break
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        chunks.append(chunk.decode(errors="replace"))
    return "".join(chunks)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sizer", default="target/debug/sizer")
    args = parser.parse_args()
    sizer = os.path.abspath(args.sizer)

    bus = subprocess.run(
        ["dbus-daemon", "--session", "--fork", "--print-address=1", "--print-pid=1"],
        capture_output=True,
        text=True,
        check=True,
    )
    address, bus_pid = bus.stdout.splitlines()[:2]
    workdir = tempfile.mkdtemp(prefix="sizer-daemon-e2e-")
    env = dict(os.environ)
    env["DBUS_SESSION_BUS_ADDRESS"] = address
    env["XDG_CONFIG_HOME"] = workdir
    config_path = os.path.join(workdir, "sizer", "config.toml")
    daemon = None
    failures = []

    try:
        flags = (
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
            | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
        )
        connection = Gio.DBusConnection.new_for_address_sync(address, flags, None, None)
        connection.call_sync(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            GLib.Variant("(su)", (BUS_NAME, 0)),
            None,
            Gio.DBusCallFlags.NONE,
            -1,
            None,
        )
        shell = FakeShell(connection)

        daemon = subprocess.Popen(
            [sizer, "daemon"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        if not pump(8, lambda: bool(shell.payloads)):
            failures.append("daemon never pushed presets on startup")
        else:
            print("PASS startup push (%d presets)" % len(preset_entries(shell.payloads[-1])))

        before = len(shell.payloads)
        added = subprocess.run(
            [
                sizer,
                "preset",
                "add",
                "--label",
                "1080p",
                "--size",
                "1920x1080",
                "--group",
                "16:9",
                "--anchor",
                "center",
            ],
            env=env,
            capture_output=True,
            text=True,
        )
        if added.returncode != 0:
            failures.append("preset add failed: " + added.stderr.strip())
        pushed = pump(10, lambda: any("1080p" in p for p in shell.payloads[before:]))
        if not pushed:
            failures.append("daemon did not push the added preset")
        else:
            payload = preset_entries(shell.payloads[-1])
            entry = next(item for item in payload if item["label"] == "1080p")
            if entry.get("group") != "16:9":
                failures.append("pushed preset lost its group: %r" % entry.get("group"))
            else:
                print("PASS add reached the daemon (group=%s)" % entry["group"])

        before = len(shell.payloads)
        renamed = subprocess.run(
            [
                sizer,
                "preset",
                "set",
                "16:9 / 1080p",
                "--label",
                "1080p-renamed",
                "--group",
                "16:9 / UHD",
            ],
            env=env,
            capture_output=True,
            text=True,
        )
        if renamed.returncode != 0:
            failures.append("preset set failed: " + renamed.stderr.strip())
        pushed = pump(10, lambda: any("1080p-renamed" in p for p in shell.payloads[before:]))
        if not pushed:
            failures.append("daemon did not push the renamed preset")
        else:
            payload = preset_entries(shell.payloads[-1])
            entry = next(item for item in payload if item["label"] == "1080p-renamed")
            if entry.get("group") != "16:9 / UHD":
                failures.append("group change not pushed: %r" % entry.get("group"))
            else:
                print("PASS rename and group change reached the daemon")

        removed = subprocess.run(
            [sizer, "preset", "remove", "16:9 / UHD / 1080p-renamed"],
            env=env,
            capture_output=True,
            text=True,
        )
        if removed.returncode != 0:
            failures.append("preset remove failed: " + removed.stderr.strip())
        gone = pump(10, lambda: bool(shell.payloads) and "1080p-renamed" not in shell.payloads[-1])
        if gone:
            print("PASS remove reached the daemon")
        else:
            failures.append("daemon did not drop the removed preset")

        # The extension skips entries it cannot honour. The daemon must report
        # them, and the difference between the configured and the kept list must
        # not look like "the shell lost the presets" on every tick.
        added_skip = subprocess.run(
            [sizer, "preset", "add", "--label", "skip-me", "--size", "640x480"],
            env=env,
            capture_output=True,
            text=True,
        )
        if added_skip.returncode != 0:
            failures.append("preset add (skipped) failed: " + added_skip.stderr.strip())
        pump(8, lambda: any("skip-me" in p for p in shell.payloads))
        pump(1.5)
        output = drain_output(daemon)
        if 'skipped "skip-me"' in output:
            print("PASS the daemon reported the skipped preset")
        else:
            failures.append("daemon did not report the skipped preset: " + output[-300:])
        pushes_before = len(shell.payloads)
        pump(2.5)
        drain_output(daemon)
        if len(shell.payloads) != pushes_before:
            failures.append(
                "daemon re-pushed skipped presets %d times" % (len(shell.payloads) - pushes_before)
            )
        else:
            print("PASS skipped presets do not cause a re-push loop")

        if os.path.exists(config_path):
            saved = open(config_path, encoding="utf-8").read()
            if 'version = 1' not in saved:
                failures.append("configuration file was not written")
        else:
            failures.append("configuration file is missing: " + config_path)
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        os.kill(int(bus_pid), signal.SIGTERM)
        shutil.rmtree(workdir, ignore_errors=True)

    if failures:
        for failure in failures:
            print("FAIL " + failure)
        return 1
    print("PASS preset CLI changes reach a running daemon")
    return 0


if __name__ == "__main__":
    sys.exit(main())
