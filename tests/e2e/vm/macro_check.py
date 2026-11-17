#!/usr/bin/env python3
"""Prove the macro path on the real Ubuntu 26.04 desktop (GNOME 50).

Runs inside the guest session (uid 1000, real mutter, installed deb):

  XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
      python3 macro_check.py

It refuses silent fallbacks: every D-Bus member the macro executor relies on is
probed and reported, and the two-window layout is read back through a second
`ListWindows` call rather than trusting the command's own output.
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gnome_extension as G  # noqa: E402

SIZER = os.environ.get("SIZER", "/usr/bin/sizer")
CLIENT = os.environ.get("CLIENT", "/tmp/sizer-e2e/gnome_client.py")
PREFIX = "MACRO-2604"
TIMEOUT = 120.0


def windows():
    return G.call("ListWindows")


def window_by_title(title, deadline):
    return G.find_window(title, deadline)


def wait_mapped(title, deadline):
    """A mapped window has a non-zero client rect; a listed one may not yet."""
    window = window_by_title(title, deadline)
    while time.time() < deadline:
        client = window.get("client") or {}
        if client.get("w", 0) > 0 and client.get("h", 0) > 0:
            return window
        time.sleep(0.1)
        window = next((entry for entry in windows()
                       if entry.get("title") == title), window)
    return window


def poll(predicate, deadline, interval=0.1):
    while time.time() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return None


def probe_capabilities():
    """Every member and field the macro host uses, probed on this GNOME version."""
    listing = windows()
    fields = set(listing[0]) if listing else set()
    probes = {
        "stable_sequence": {"present": "stable_sequence" in fields},
        "workspace": {"present": "workspace" in fields},
    }
    checks = {
        "MoveResize": ("no-such-window", 0, 0, 100, 100, "client"),
        "SetMinimized": ("no-such-window", True),
        "Activate": ("no-such-window",),
        "LastWindow": (),
    }
    for method, args in checks.items():
        reply = G.call_quiet(method, args)
        if method == "LastWindow":
            exists = reply.get("ok") is True and "window" in reply
        else:
            # An existing method answers "no_such_window" for the probe id;
            # a missing one comes back as a D-Bus unknown-method error.
            exists = reply.get("ok") is False and \
                reply.get("error") == "no_such_window"
        probes[method] = {"present": bool(exists), "reply": reply}
    return probes


def spawn(title, width, height):
    log = open(f"/tmp/{title}.log", "wb")
    return subprocess.Popen(
        [sys.executable, CLIENT, "--title", title,
         "--width", str(width), "--height", str(height)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)


def run_macro(text):
    result = subprocess.run([SIZER, "macro", text, "--json"],
                            capture_output=True, text=True, timeout=90)
    report = None
    if result.stdout.strip().startswith("{"):
        report = json.loads(result.stdout)
    return result.returncode, report, result.stderr


def main():
    deadline = time.time() + TIMEOUT
    capabilities = probe_capabilities()
    missing = [name for name, probe in capabilities.items() if not probe["present"]]

    clients = [spawn(f"{PREFIX}-A", 300, 220), spawn(f"{PREFIX}-B", 300, 220)]
    try:
        first = wait_mapped(f"{PREFIX}-A", deadline)
        second = wait_mapped(f"{PREFIX}-B", deadline)
        shell = G.call("ShellInfo")
        workspaces = {
            first["title"]: first.get("workspace"),
            second["title"]: second.get("workspace"),
        }
        workspace_ok = all(
            isinstance(value, int) and value == shell.get("current_workspace")
            for value in workspaces.values())
        sequences = [first.get("stable_sequence"), second.get("stable_sequence")]
        sequence_ok = all(isinstance(value, int) for value in sequences) and \
            len(set(sequences)) == 2

        macro = (
            "resize : proc[python3]:cap[MACRO-2604-]:desktop*\n"
            "t = 100\n"
            "l = 80 + 380 * ($_index - 1)\n"
            "w = 360\n"
            "h = 280"
        )
        returncode, report, stderr = run_macro(macro)
        outcomes = {outcome["title"]: outcome
                    for outcome in (report or {}).get("windows", [])}
        expectations = {
            outcome["title"]: {
                "x": 80 + 380 * (outcome["index"] - 1),
                "y": 100,
                "w": 360,
                "h": 280,
            }
            for outcome in (report or {}).get("windows", [])
        }
        live = {window["title"]: window["client"]
                for window in windows()
                if str(window.get("title", "")).startswith(PREFIX)}
        layout_ok = (
            returncode == 0
            and bool(report)
            and report.get("backend") == "gnome"
            and report.get("ok") is True
            and len(report.get("windows", [])) == 2
            and sorted(outcome["index"] for outcome in report["windows"]) == [1, 2]
            and all(
                outcome.get("count") == 2
                and outcome.get("exact") is True
                and outcome.get("requested") == expectations[outcome["title"]]
                and outcome.get("client") == expectations[outcome["title"]]
                for outcome in report["windows"]
            )
            and live == expectations
        )

        # SetMinimized: the state has to flip in mutter, not just be accepted.
        target = outcomes.get(f"{PREFIX}-A", {})
        target_id = target.get("id")
        minimized_on = minimized_off = False
        if target_id:
            G.call("SetMinimized", (target_id, True))
            minimized_on = bool(poll(
                lambda: (G.call_quiet("GetWindow", (target_id,))
                         .get("window") or {}).get("minimized") is True,
                time.time() + 5))
            G.call("SetMinimized", (target_id, False))
            minimized_off = bool(poll(
                lambda: (G.call_quiet("GetWindow", (target_id,))
                         .get("window") or {}).get("minimized") is False,
                time.time() + 5))

        # Activate + LastWindow: focus moves, and `last` names the other window.
        focused_ok = last_ok = False
        if target_id:
            G.call("Activate", (target_id,))
            focused = poll(
                lambda: next((window for window in windows()
                              if window.get("id") == target_id
                              and window.get("focused")), None),
                time.time() + 5)
            focused_ok = focused is not None
            previous = G.call("LastWindow")
            last_ok = previous.get("ok") is True and \
                (previous.get("window") or {}).get("id") not in (None, target_id)

        # MoveResize in frame basis lands exactly as asked.
        resize_ok = False
        if target_id:
            G.call("MoveResize", (target_id, 200, 200, 500, 400, "frame"))
            settled = poll(
                lambda: next((window for window in windows()
                              if window.get("id") == target_id
                              and window.get("frame") ==
                              {"x": 200, "y": 200, "w": 500, "h": 400}), None),
                time.time() + 5)
            resize_ok = settled is not None

        report_out = {
            "capabilities": capabilities,
            "missing": missing,
            "stable_sequence_ok": sequence_ok,
            "workspace": {"values": workspaces,
                          "current": shell.get("current_workspace"),
                          "ok": workspace_ok},
            "macro": {"returncode": returncode, "report": report,
                      "stderr": stderr[-400:], "ok": layout_ok},
            "live": live,
            "expected": expectations,
            "state": {
                "SetMinimized": {"on": minimized_on, "off": minimized_off},
                "Activate": focused_ok,
                "LastWindow": last_ok,
                "MoveResize": resize_ok,
            },
        }
        print(json.dumps(report_out, indent=2, default=str))
        state = report_out["state"]
        failed = [
            name for name, value in state.items()
            if (isinstance(value, dict) and not all(value.values()))
            or (not isinstance(value, dict) and not value)
        ]
        ok = (
            not missing
            and sequence_ok
            and workspace_ok
            and layout_ok
            and not failed
        )
        if not ok:
            print(f"FAILED: missing={missing} failed_state={failed}", file=sys.stderr)
            return 1
        print("MACRO_2604_OK")
        return 0
    finally:
        for client in clients:
            client.terminate()
        for client in clients:
            try:
                client.wait(timeout=5)
            except subprocess.TimeoutExpired:
                client.kill()


if __name__ == "__main__":
    sys.exit(main())
