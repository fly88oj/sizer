#!/usr/bin/env python3
"""Assertions against the real 26.04 desktop session (not a harness-owned shell).

Run it inside the VM with the session bus:
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \\
  XDG_RUNTIME_DIR=/run/user/1000 python3 session_check.py keys|state|apply <row>

It reuses the harness helpers, so a preset is judged by the same rule as CI: the
reported client rect must equal the requested one and `exact` must be true.
"""

import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gnome_extension as G  # noqa: E402


def state():
    return {
        "shell": G.call_quiet("ShellInfo"),
        "keys": G.call_quiet("KeybindingState"),
        "menu": G.call_quiet("MenuState"),
        "windows": G.call_quiet("ListWindows"),
    }


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "state"
    if command == "wait-bus":
        ok = G.wait_name(time.time() + (float(sys.argv[2]) if len(sys.argv) > 2 else 60))
        print(json.dumps({"bus": ok, "state": state() if ok else None}))
        return 0 if ok else 1
    if command == "state":
        print(json.dumps(state(), default=str))
        return 0
    if command == "keys":
        print(json.dumps({"keys": G.call_quiet("KeybindingState"),
                          "menu": G.call_quiet("MenuState")}, default=str))
        return 0
    if command == "presets":
        # The prove script pushes a preset that fits the VM screen, so the landed
        # assertion does not depend on the built-in defaults. `-` reads the payload
        # from stdin, which keeps the JSON away from the remote shell's quoting.
        payload = sys.stdin.read() if len(sys.argv) < 3 or sys.argv[2] == "-" else sys.argv[2]
        print(json.dumps({"pushed": G.call("SetPresets", (payload,))}, default=str))
        return 0
    if command == "apply":
        row = int(sys.argv[2]) if len(sys.argv) > 2 else 0
        presets = G.call("MenuState").get("presets") or []
        # A popup from the keybinding check may still be up; dropping it first keeps
        # the focused window the one that will receive the preset.
        G.call("CloseMenu")
        # The menu targets the focused window, so read its id before activating:
        # the landed check needs the window that received the request.
        focused = next((window for window in G.call("ListWindows") if window.get("focused")), None)
        win_id = (focused or {}).get("id")
        G.call("ShowMenu", (300, 200))
        activated = G.call("ActivateMenuRow", (row,))
        label = presets[row] if row < len(presets) else None
        result, window = G.wait_applied(win_id, time.time() + 6, label=label,
                                        activated=activated)
        landed = bool(window) and G.preset_landed(activated, result, window)
        print(json.dumps({"presets": presets, "activated": activated, "label": label,
                          "result": G.applied_detail(result),
                          "window": G.window_detail(window) if window else None,
                          "landed": landed}, default=str))
        return 0
    print(f"unknown command {command}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
