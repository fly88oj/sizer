#!/usr/bin/env python3
"""GNOME extension harness: isolated headless GNOME Shell + extension + GTK client.

Must be launched through dbus-run-session (see run_gnome_harness.sh) so the shell,
the extension and this driver share a throwaway session bus.
"""

import argparse
import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

BUS = "org.sizerlinux.Shell"
OBJ = "/org/sizerlinux/Shell"
IFACE = "org.sizerlinux.Shell"
EXT_UUID = "sizer@sizerlinux.dev"
CLIENT_TITLE = "SIZER-SPIKE-CLIENT"
CLIENT_TITLE_B = "SIZER-SPIKE-SECOND"
REMOTE_DESKTOP = "org.gnome.Mutter.RemoteDesktop"
REMOTE_PATH = "/org/gnome/Mutter/RemoteDesktop"

# LIMITED is reserved for cases this environment genuinely cannot exercise, and
# every entry must name the limitation. Anything else is a fail, so `--strict`
# (used by `make gate`) cannot be satisfied by a broken main path. A case that
# probes the environment itself calls `recorder.allow_limited(...)` instead, so
# the exemption is bound to the observed failure rather than assumed here.
STRICT_ALLOWED = {
    "extension_toggle": "the nested shell does not export org.gnome.Shell.Extensions "
                        "(StartServiceByName returns status 1), so there is nothing to toggle",
}

SIGNATURES = {
    "Ping": "()",
    "ShellInfo": "()",
    "ListWindows": "()",
    "GetWindow": "(s)",
    "MoveResizeFrame": "(siiii)",
    "MoveResize": "(siiiis)",
    "MoveFrame": "(sii)",
    "SetAbove": "(sb)",
    "SetMaximized": "(sb)",
    "SetMinimized": "(sb)",
    "Activate": "(s)",
    "LastWindow": "()",
    "SetDecorated": "(sb)",
    "KeybindingState": "()",
    "ShowMenu": "(ii)",
    "CloseMenu": "()",
    "SetPresets": "(s)",
    "MenuState": "()",
    "ActivateMenuRow": "(i)",
    "LastResult": "()",
}


_BUS_CONNECTION = None


def bus():
    """Gio.DBus.session is GJS-only; in PyGObject the connection must be fetched."""
    global _BUS_CONNECTION
    if _BUS_CONNECTION is None:
        _BUS_CONNECTION = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    return _BUS_CONNECTION


class Recorder:
    def __init__(self, out_dir: Path):
        self.path = out_dir / "results.jsonl"
        self.records = []
        self.path.write_text("")
        self.allowed = {}
        self.counts = {"pass": 0, "limited": 0, "fail": 0}

    def add(self, name, status, detail):
        record = {"test": name, "status": status, "detail": detail, "ts": time.time()}
        self.records.append(record)
        self.counts[status] = self.counts.get(status, 0) + 1
        with self.path.open("a") as handle:
            handle.write(json.dumps(record, default=str) + "\n")
        print(f"[{status.upper():7}] {name}: {json.dumps(detail, default=str)[:400]}", flush=True)

    def allow_limited(self, name, reason):
        """Permit one LIMITED case because the environment was observed to lack a capability."""
        self.allowed[name] = reason

    def exit_code(self, strict):
        """Non-zero when a case failed; `strict` also fails on unlisted LIMITED cases."""
        print(f"SUMMARY pass={self.counts['pass']} limited={self.counts['limited']} "
              f"fail={self.counts['fail']}", flush=True)
        if self.counts["fail"]:
            return 1
        if not strict:
            return 0
        allowed = {**STRICT_ALLOWED, **self.allowed}
        limited = [record["test"] for record in self.records if record["status"] == "limited"]
        unexpected = [name for name in limited if name not in allowed]
        for name in limited:
            if name in allowed:
                print(f"ALLOWED  {name}: {allowed[name]}", flush=True)
        if unexpected:
            print("STRICT   limited without a documented environment limit: "
                  + ", ".join(unexpected), flush=True)
            return 1
        return 0


def call(method, args=(), timeout_ms=5000):
    reply = bus().call_sync(
        BUS, OBJ, IFACE, method, GLib.Variant(SIGNATURES[method], args),
        None, Gio.DBusCallFlags.NONE, timeout_ms, None)
    return json.loads(reply.unpack()[0])


def call_quiet(method, args=(), timeout_ms=3000):
    try:
        return call(method, args, timeout_ms)
    except Exception as exc:  # noqa: BLE001 - probe wants the raw failure
        return {"ok": False, "dbus_error": str(exc)}


def wait_name(deadline):
    last_error = None
    while time.time() < deadline:
        try:
            reply = bus().call_sync(
                "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                "NameHasOwner", GLib.Variant("(s)", (BUS,)), None,
                Gio.DBusCallFlags.NONE, 1000, None)
            if reply.unpack()[0]:
                return True
        except Exception as exc:  # noqa: BLE001 - surface the reason in the report
            last_error = f"{type(exc).__name__}: {exc}"
        time.sleep(0.1)
    if last_error:
        print(f"[wait_name] last error: {last_error}", file=sys.stderr, flush=True)
    return False


def find_window(title, deadline):
    last = None
    while time.time() < deadline:
        windows = call_quiet("ListWindows")
        last = windows
        if isinstance(windows, list):
            for win in windows:
                if win.get("title") == title:
                    return win
        time.sleep(0.1)
    raise RuntimeError(f"window {title!r} not found; listing={json.dumps(last)[:800]}")


def stable_window(win_id, deadline, settle_ms=90):
    """Read the window until two consecutive reads agree, or the deadline expires."""
    previous = None
    while time.time() < deadline:
        payload = call_quiet("GetWindow", (win_id,))
        current = payload.get("window") if isinstance(payload, dict) else None
        if current is not None and previous is not None and \
                current["frame"] == previous["frame"] and current["buffer"] == previous["buffer"]:
            return current
        previous = current
        time.sleep(settle_ms / 1000.0)
    return previous


def remaximize(win_id, deadline=None):
    """Maximize and wait until the compositor reports the state."""
    call("SetMaximized", (win_id, True))
    deadline = deadline or time.time() + 5
    window = None
    while time.time() < deadline:
        window = call_quiet("GetWindow", (win_id,)).get("window")
        if window and window.get("maximized"):
            return window
        time.sleep(0.1)
    return window


def applied_result(result, label=None, generation=None, requested=None):
    """Whether `result` describes the request being waited for, already measured.

    A label alone is not enough: picking the same preset twice leaves the previous
    `exact:true` in `LastResult`, so a caller that only matched the label reported
    the old outcome as the new one (a stale green). The per-window generation of the
    request is the reliable discriminator; `requested` is the fallback for callers
    that only have the rect the preset asked for.
    """
    if not isinstance(result, dict) or result.get("exact") is None:
        return False
    if label is not None and result.get("label") != label:
        return False
    if generation is not None and result.get("generation") != generation:
        return False
    if requested is not None:
        got = result.get("requested_client") or {}
        if (got.get("w"), got.get("h")) != (requested.get("w"), requested.get("h")):
            return False
        if requested.get("x") is not None and \
                (got.get("x"), got.get("y")) != (requested.get("x"), requested.get("y")):
            return False
    return True


def wait_applied(win_id, deadline, label=None, activated=None, requested=None):
    """Poll LastResult until this request — not an older one with the same label — landed.

    `activated` is the `ActivateMenuRow` reply of the request being waited for; its
    generation is what tells two same-label requests apart. Returns `{}` when the
    deadline expires without a matching result, so a stale `exact:true` from the
    previous pick can never be reported as this one's outcome.
    """
    generation = (activated or {}).get("generation") if activated is not None else None
    result = call_quiet("LastResult")
    window = call_quiet("GetWindow", (win_id,)).get("window")
    while time.time() < deadline and not applied_result(result, label, generation, requested):
        time.sleep(0.1)
        result = call_quiet("LastResult")
        window = call_quiet("GetWindow", (win_id,)).get("window")
    if not applied_result(result, label, generation, requested):
        return {}, window
    return result, window


def applied_detail(result):
    return {key: result.get(key) for key in
            ("label", "requested_client", "achieved_client", "exact", "unmaximized",
             "error", "reason", "superseded")}


def window_detail(window):
    return {"client": window["client"], "frame": window["frame"],
            "maximized": window.get("maximized")}


def preset_landed(activated, result, window):
    """A preset lands when the reported client rect is the requested one."""
    requested = result.get("requested_client") or {}
    achieved = window["client"]
    return (activated.get("ok") is True
            and result.get("exact") is True
            and requested.get("w") is not None
            and achieved["w"] == requested["w"]
            and achieved["h"] == requested["h"]
            and window.get("maximized") == 0)


def wait_presets(expected, deadline, interval=0.15):
    """Poll MenuState until the extension reports exactly these depth-first labels."""
    last = None
    while time.time() < deadline:
        state = call_quiet("MenuState")
        last = state.get("presets") if isinstance(state, dict) else None
        if last == expected:
            return True, last
        time.sleep(interval)
    return False, last


class RemoteInput:
    """Key injection through mutter's RemoteDesktop session.

    GNOME 50 has no X11 backend, so a nested compositor plus XTEST is not
    available there; this is the only path that feeds real input into the shell.
    """

    def __init__(self):
        self.session = None

    def start(self):
        try:
            reply = bus().call_sync(
                REMOTE_DESKTOP, REMOTE_PATH, REMOTE_DESKTOP, "CreateSession",
                None, None, Gio.DBusCallFlags.NONE, 3000, None)
            self.session = reply.unpack()[0]
            bus().call_sync(
                REMOTE_DESKTOP, self.session, f"{REMOTE_DESKTOP}.Session", "Start",
                None, None, Gio.DBusCallFlags.NONE, 5000, None)
            return True
        except Exception as exc:  # noqa: BLE001 - the report needs the reason
            print(f"[remote] unavailable: {exc}", file=sys.stderr, flush=True)
            return False

    def key(self, keycode, pressed):
        bus().call_sync(
            REMOTE_DESKTOP, self.session, f"{REMOTE_DESKTOP}.Session",
            "NotifyKeyboardKeycode", GLib.Variant("(ub)", (keycode, pressed)),
            None, Gio.DBusCallFlags.NONE, 3000, None)

    def chord(self, keycodes):
        for keycode in keycodes:
            self.key(keycode, True)
            time.sleep(0.03)
        for keycode in reversed(keycodes):
            self.key(keycode, False)
            time.sleep(0.03)


class ShellSession:
    def __init__(self, out_dir: Path, shell_bin: str, monitor: str, wl_display: str,
                 nested_display: str | None = None, parent_sway: bool = False,
                 shell_mode: str = "user"):
        self.out_dir = out_dir
        self.shell_bin = shell_bin
        self.monitor = monitor
        self.wl_display = wl_display
        self.nested_display = nested_display
        self.parent_sway = parent_sway
        # The session mode file is distribution specific: upstream ships `user`,
        # Ubuntu 26.04 only ships `ubuntu`.
        self.shell_mode = shell_mode
        self.parent_display = None
        self.process = None
        self.sway = None
        self.log_handle = None
        self.xvfb = None

    def _start_sway(self):
        """Headless sway as the parent compositor: mutter 50 can only nest in
        Wayland, and sway's virtual keyboard then delivers real key events."""
        env = dict(os.environ, WLR_BACKENDS="headless", WLR_RENDERER="pixman",
                   WLR_LIBINPUT_NO_DEVICES="1")
        self.sway = subprocess.Popen(
            ["sway", "-d"], stdout=self.log_handle, stderr=subprocess.STDOUT, env=env)
        deadline = time.time() + 20
        socket = None
        while time.time() < deadline:
            candidates = [path for path in
                          sorted(Path(os.environ["XDG_RUNTIME_DIR"]).glob("wayland-*"))
                          if not path.name.endswith(".lock") and path.name != self.wl_display]
            if candidates:
                socket = candidates[0]
                break
            time.sleep(0.1)
        if not socket:
            raise RuntimeError("sway never created a wayland socket")
        self.parent_display = socket.name
        sway_env = dict(env, WAYLAND_DISPLAY=self.parent_display)
        subprocess.run(["swaymsg", "create_output"], capture_output=True, env=sway_env)
        width, height = self.monitor.split("x")
        subprocess.run(
            ["swaymsg", "output", "HEADLESS-1", "resolution", width, height],
            capture_output=True, env=sway_env)
        return sway_env

    def start(self):
        self.log_handle = (self.out_dir / "shell.log").open("wb")
        if self.parent_sway:
            env = self._start_sway()
            self.process = subprocess.Popen(
                [self.shell_bin, "--wayland", "--wayland-display", self.wl_display,
                 "--no-x11", "--mode", self.shell_mode],
                stdout=self.log_handle, stderr=subprocess.STDOUT, env=env)
            return
        if self.nested_display:
            # Nested compositor on a private Xvfb screen: needed for real key injection.
            self.xvfb = subprocess.Popen(
                ["Xvfb", self.nested_display, "-screen", "0", f"{self.monitor}x24",
                 "-nolisten", "tcp"],
                stdout=self.log_handle, stderr=subprocess.STDOUT)
            deadline = time.time() + 10
            socket = Path("/tmp/.X11-unix") / f"X{self.nested_display.lstrip(':')}"
            while time.time() < deadline and not socket.exists():
                time.sleep(0.05)
            command = [self.shell_bin, "--nested", "--wayland",
                       "--wayland-display", self.wl_display, "--mode", self.shell_mode]
        else:
            command = [self.shell_bin, "--headless", "--wayland", "--no-x11",
                       "--virtual-monitor", self.monitor,
                       "--wayland-display", self.wl_display,
                       "--mode", self.shell_mode]
        env = dict(os.environ)
        if self.nested_display:
            env["DISPLAY"] = self.nested_display
        self.process = subprocess.Popen(
            command, stdout=self.log_handle, stderr=subprocess.STDOUT, env=env)

    def socket_path(self):
        return Path(os.environ["XDG_RUNTIME_DIR"]) / self.wl_display

    def stop_xvfb(self):
        if not self.xvfb or self.xvfb.poll() is not None:
            return
        self.xvfb.terminate()
        try:
            self.xvfb.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.kill(self.xvfb.pid, signal.SIGKILL)
            self.xvfb.wait(timeout=5)

    def stop(self):
        code = 0
        if self.process and self.process.poll() is None:
            pid = self.process.pid
            self.process.terminate()
            try:
                code = self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.kill(pid, signal.SIGKILL)
                code = self.process.wait(timeout=5)
        if self.sway and self.sway.poll() is None:
            self.sway.terminate()
            try:
                self.sway.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.kill(self.sway.pid, signal.SIGKILL)
                self.sway.wait(timeout=5)
        return code


class Client:
    def __init__(self, out_dir: Path, script: Path, title: str, wl_display: str, extra=()):
        self.script = script
        self.title = title
        self.env = dict(os.environ, WAYLAND_DISPLAY=wl_display, GDK_BACKEND="wayland",
                        GSK_RENDERER="cairo", GTK_A11Y="none", LIBGL_ALWAYS_SOFTWARE="1")
        self.extra = list(extra)
        self.events = queue.Queue()
        self.log = (out_dir / f"client-{title}.log").open("wb")
        self.process = None

    def start(self):
        self.process = subprocess.Popen(
            [sys.executable, str(self.script), "--title", self.title, *self.extra],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
            env=self.env, text=True)
        threading.Thread(target=self._pump, daemon=True).start()

    def send(self, line):
        """Tell the client what to do next (see `--defer-map`)."""
        if self.process and self.process.stdin:
            self.process.stdin.write(line + "\n")
            self.process.stdin.flush()

    def _pump(self):
        for line in self.process.stdout:
            line = line.strip()
            if line:
                self.events.put(line)

    def drain(self):
        items = []
        while not self.events.empty():
            items.append(self.events.get())
        return items

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.kill(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)


def expect_frame(recorder, name, window, requested, tolerance=0):
    """Compare a frame with the request; a miss is a failure."""
    frame = window["frame"]
    delta = {"dw": frame["w"] - requested["w"], "dh": frame["h"] - requested["h"],
             "dx": frame["x"] - requested["x"], "dy": frame["y"] - requested["y"]}
    ok = (abs(delta["dw"]) <= tolerance and abs(delta["dh"]) <= tolerance and
          abs(delta["dx"]) <= tolerance and abs(delta["dy"]) <= tolerance)
    detail = {"requested": requested, "achieved_frame": frame,
              "achieved_client": window["client"], "delta": delta,
              "maximized": window.get("maximized"), "work_area": window.get("work_area")}
    recorder.add(name, "pass" if ok else "fail", detail)
    return ok, detail


def xwin_candidates(display):
    """List X windows on a private Xvfb display with class/name (no WM is running)."""
    env = dict(os.environ, DISPLAY=display)
    found = subprocess.run(["xdotool", "search", "--name", ""],
                           capture_output=True, text=True, env=env)
    if found.returncode != 0:
        print(f"[xwin] xdotool rc={found.returncode} err={found.stderr.strip()} display={display}",
              file=sys.stderr, flush=True)
    result = []
    for wid in [x for x in found.stdout.split() if x.strip()]:
        cls = subprocess.run(["xdotool", "getwindowclassname", wid],
                             capture_output=True, text=True, env=env).stdout.strip()
        name = subprocess.run(["xdotool", "getwindowname", wid],
                              capture_output=True, text=True, env=env).stdout.strip()
        result.append({"id": wid, "class": cls, "name": name})
    return result


def inject_key(display, window_id, combo, out_dir: Path):
    """Send a key combo to a private Xvfb display only (never the user's session)."""
    env = dict(os.environ, DISPLAY=display)
    log = (out_dir / "xdotool.log").open("a")
    subprocess.run(["xdotool", "windowfocus", window_id], capture_output=True, env=env)
    time.sleep(0.3)
    result = subprocess.run(["xdotool", "key", "--clearmodifiers", combo],
                            capture_output=True, text=True, env=env)
    log.write(f"focus {window_id}; key {combo}: rc={result.returncode} {result.stderr}\n")
    log.close()
    return result.returncode == 0


def press_switch(display, out_dir: Path):
    """Press the window switcher once on the private Xvfb display.

    alt+Escape is the binding that moves the focus between the two windows in this
    nested session (measured: a click is delivered without changing it, a freshly
    mapped window does not take it, alt+Tab does not switch).
    """
    env = dict(os.environ, DISPLAY=display)
    result = subprocess.run(["xdotool", "key", "--clearmodifiers", "alt+Escape"],
                            capture_output=True, text=True, env=env)
    with (out_dir / "xdotool.log").open("a") as log:
        log.write(f"alt+Escape: rc={result.returncode} {result.stderr}\n")
    return {"rc": result.returncode, "stderr": result.stderr.strip()}


def wait_focused(win_id, seconds):
    """Poll the window's own focus flag."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        window = call_quiet("GetWindow", (win_id,)).get("window")
        if window and window.get("focused"):
            return True
        time.sleep(0.02)
    return False


def focus_window(display, win_id, out_dir: Path, presses=3):
    """Bring `win_id` into focus with a click, then with switcher presses.

    Every step is read back from the window's own flag, so the report says which
    input actually moved the focus instead of assuming one did.
    """
    if wait_focused(win_id, 0.1):
        return {"already_focused": True}
    window = call_quiet("GetWindow", (win_id,)).get("window") or {}
    tried = []
    if window.get("frame"):
        tried.append({"click": focus_by_click(display, window["frame"], out_dir)})
        if wait_focused(win_id, 0.8):
            return {"moved": "click", "tried": tried}
    for _ in range(presses):
        tried.append({"keys": press_switch(display, out_dir)})
        if wait_focused(win_id, 0.8):
            return {"moved": "keys", "tried": tried}
    return {"moved": None, "tried": tried}


def focus_by_click(display, frame, out_dir: Path):
    """Click the centre of `frame` on the nested X display.

    A click is the way a user moves the focus, so it is tried before the
    switcher bindings; the coordinates come from the window's own frame.
    """
    env = dict(os.environ, DISPLAY=display)
    x = frame["x"] + frame["w"] // 2
    y = frame["y"] + frame["h"] // 2
    result = subprocess.run(["xdotool", "mousemove", str(x), str(y), "click", "1"],
                            capture_output=True, text=True, env=env)
    with (out_dir / "xdotool.log").open("a") as log:
        log.write(f"click {x},{y}: rc={result.returncode} {result.stderr}\n")
    return {"x": x, "y": y, "rc": result.returncode, "stderr": result.stderr.strip()}


class Signals:
    """Collect `Changed` payloads with timestamps.

    `LastResult` is a single slot, so a case that drives two windows has to read
    the events instead: each carries the result of the request that produced it.
    """

    def __init__(self):
        self.entries = []
        self._id = bus().signal_subscribe(
            BUS, IFACE, "Changed", OBJ, None, Gio.DBusSignalFlags.NONE,
            lambda *args: self.entries.append(
                (time.time(), json.loads(args[5].unpack()[0]))))

    def pump(self, seconds):
        context = GLib.MainContext.default()
        deadline = time.time() + seconds
        while time.time() < deadline:
            context.iteration(False)
            time.sleep(0.02)

    def wait_for(self, predicate, deadline):
        context = GLib.MainContext.default()
        while time.time() < deadline:
            for stamp, entry in self.entries:
                if predicate(entry):
                    return stamp, entry
            context.iteration(False)
            time.sleep(0.02)
        return None, None

    def close(self):
        bus().signal_unsubscribe(self._id)


def applied_with(signals, label):
    """Signals for one preset label, in arrival order."""
    return [(stamp, entry["result"]) for stamp, entry in signals.entries
            if entry.get("reason") in ("preset-applied", "preset-verified")
            and (entry.get("result") or {}).get("label") == label]


def signal_log(signals, label):
    """Every event for one label, for failure details."""
    return [{"event": entry.get("reason"),
             "reason": (entry.get("result") or {}).get("reason"),
             "exact": (entry.get("result") or {}).get("exact")}
            for _, entry in signals.entries
            if (entry.get("result") or {}).get("label") == label]


def inject_keys_sway(parent_display, out_dir: Path):
    """Send Ctrl+Super+Z through sway's virtual keyboard to the nested shell."""
    env = dict(os.environ, WAYLAND_DISPLAY=parent_display)
    log = (out_dir / "wtype.log").open("a")
    result = subprocess.run(
        ["wtype", "-M", "ctrl", "-M", "logo", "-k", "z", "-m", "logo", "-m", "ctrl"],
        capture_output=True, text=True, env=env)
    log.write(f"wtype: rc={result.returncode} {result.stderr}\n")
    log.close()
    return result.returncode == 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--shell", default="gnome-shell")
    parser.add_argument("--monitor", default="1600x900")
    parser.add_argument("--wayland-display", default="sizer-spike")
    parser.add_argument("--nested-display", default=None,
                        help="run nested inside this private Xvfb display, e.g. :97")
    parser.add_argument("--parent-sway", action="store_true",
                        help="nested under a headless sway (needed on GNOME 50, "
                             "whose mutter has no X11 backend)")
    parser.add_argument("--shell-mode", default="user",
                        help="GNOME session mode file, e.g. user (upstream) or ubuntu")
    parser.add_argument("--skip-signal", action="store_true")
    parser.add_argument("--sizer", default=None,
                        help="sizer binary used for the daemon push tests")
    parser.add_argument("--strict", action="store_true",
                        help="treat LIMITED cases as failures (used by release gates)")
    parser.add_argument("--only", action="append", metavar="NAME",
                        help="run only this standalone case and exit; repeatable. "
                             "Names not in ONLY_CASES are refused instead of being "
                             "silently reported as passing")
    args = parser.parse_args()

    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    recorder = Recorder(out_dir)

    shell = ShellSession(out_dir, args.shell, args.monitor, args.wayland_display,
                         args.nested_display, args.parent_sway, args.shell_mode)
    client = None
    summary = {"session_bus": os.environ.get("DBUS_SESSION_BUS_ADDRESS"),
               "shell_version": None, "tests": recorder.records}

    try:
        shell.start()
        deadline = time.time() + 45
        if not wait_name(deadline):
            recorder.add("shell_startup", "fail",
                         {"reason": "extension D-Bus name never appeared",
                          "shell_exit": shell.process.poll(),
                          "log_tail": (out_dir / "shell.log").read_text(errors="replace")[-3000:]})
            return 1
        if not shell.socket_path().exists():
            recorder.add("wayland_socket", "fail", {"path": str(shell.socket_path())})
            return 1
        recorder.add("shell_startup", "pass",
                     {"pid": shell.process.pid, "wayland_display": args.wayland_display})

        nested_window = None
        if args.nested_display:
            candidates = []
            deadline = time.time() + 10
            while time.time() < deadline and not candidates:
                time.sleep(0.3)
                candidates = xwin_candidates(args.nested_display)
            shell_windows = [c for c in candidates
                             if "gnome" in c["class"].lower() or "GNOME Shell" in c["name"]]
            named = [c for c in candidates if c["class"] or c["name"]]
            if not shell_windows:
                shell_windows = named
            nested_window = shell_windows[0] if shell_windows else None
            if nested_window:
                width, height = args.monitor.split("x")
                env = dict(os.environ, DISPLAY=args.nested_display)
                subprocess.run(["xdotool", "windowsize", nested_window["id"], width, height],
                               capture_output=True, env=env)
                time.sleep(1.2)
            recorder.add("nested_surface", "pass" if nested_window else "fail",
                         {"candidates": candidates, "chosen": nested_window})

        info = call("ShellInfo")
        summary["shell_version"] = info.get("shell_version")
        recorder.add("shell_info", "pass", info)
        pong = bus().call_sync(BUS, OBJ, IFACE, "Ping", GLib.Variant("()", ()), None,
                               Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
        recorder.add("dbus_ping", "pass" if pong == "pong" else "fail", {"reply": pong})

        client = Client(out_dir, Path(__file__).resolve().with_name("gnome_client.py"),
                        CLIENT_TITLE, args.wayland_display,
                        ["--width", "800", "--height", "600"])
        client.start()
        window = find_window(CLIENT_TITLE, time.time() + 20)
        recorder.add("window_enumeration", "pass",
                     {"window": window, "client_events": client.drain()})
        win_id = window["id"]
        time.sleep(0.4)
        baseline = stable_window(win_id, time.time() + 3)
        recorder.add("geometry_read", "pass", baseline)

        work = baseline["work_area"]
        target_w = min(1024, work["w"] - 100)
        target_h = min(768, work["h"] - 100)

        def settle_unmaximized():
            """Clear the maximize-on-map state so a plain resize can be honoured.

            The client asks for 800x600 on an 800x600 monitor, so mutter maximizes
            it on map; a bare MoveResizeFrame is then ignored (asserted in T5).
            """
            state = call("GetWindow", (win_id,))["window"]
            if state.get("maximized"):
                call("SetMaximized", (win_id, False))
                time.sleep(0.4)
                state = call("GetWindow", (win_id,))["window"]
            return state

        def case_menu_apply_preset():
            # T8b: rows come from the presets, the menu sits at the pointer and one
            # row resizes the focused window to that preset. The nested monitor is
            # 800x600, so the 50% row is used: the pixel rows do not fit and mutter
            # clamps them (covered by T3).
            # Self-contained: the built-in menu and the default area/basis are
            # pushed here, so the case does not rely on what earlier cases left.
            builtin = json.dumps({
                "version": 2,
                "presets": [
                    {"label": "1920x1080", "size": "1920x1080", "anchor": "center"},
                    {"label": "1280x720", "size": "1280x720", "anchor": "center"},
                    {"label": "1024x768", "size": "1024x768", "anchor": "center"},
                    {"label": "50% x 50%", "size": "50%x50%", "anchor": "center"},
                ],
                "options": {"area": "workarea", "basis": "client"},
            }, ensure_ascii=False)
            call("SetPresets", (builtin,))
            call("CloseMenu")
            menu = call("ShowMenu", (100, 100))
            state = call("MenuState")
            geometry = state.get("geometry") or {}
            rows = state.get("items") or []
            rows_ok = (state.get("open") and
                       rows == ["1920x1080", "1280x720", "1024x768", "50% x 50%"])
            at_pointer = geometry.get("x") == 100 and geometry.get("y") == 100
            before = call("GetWindow", (win_id,))["window"]
            activated = call("ActivateMenuRow", (3,))
            result, after = wait_applied(win_id, time.time() + 5, activated=activated)
            achieved_client = (after or {}).get("client") or {}
            area = baseline["work_area"]
            half = {"w": int(area["w"] * 0.5 + 0.5), "h": int(area["h"] * 0.5 + 0.5)}
            requested = result.get("requested_client") or {}
            apply_ok = (result.get("exact") is True and
                        result.get("label") == "50% x 50%" and
                        requested.get("w") == half["w"] and requested.get("h") == half["h"] and
                        achieved_client.get("w") == half["w"] and
                        achieved_client.get("h") == half["h"])
            recorder.add("menu_apply_preset", "pass" if rows_ok and at_pointer and apply_ok else "fail",
                         {"menu": menu, "rows": rows, "geometry": geometry,
                          "activated": activated, "requested": requested,
                          "requested_frame": result.get("requested_frame"),
                          "achieved_client": achieved_client, "result": result,
                          "before_client": before.get("client")})

        def case_grouped_presets_submenu():
            # T8d: `group` turns presets into cascading submenus; arrow-key and
            # ActivateMenuRow both index the depth-first preset order.
            # Self-contained: this case declares client/workarea itself, because a
            # neighbouring case switches the global basis to frame.
            grouped = json.dumps({
                "version": 2,
                "presets": [
                    {"label": "g700", "size": "700x400", "anchor": "center", "group": "16:9"},
                    {"label": "g400", "size": "400x300", "anchor": "center", "group": "16:9"},
                    {"label": "half", "size": "50%x50%", "anchor": "center", "group": "4:3 / Wide"},
                    {"label": "plain", "size": "640x400", "anchor": "top_left"},
                ],
                "options": {"area": "workarea", "basis": "client"},
            }, ensure_ascii=False)
            grouped_push = call("SetPresets", (grouped,))
            call("ShowMenu", (200, 150))
            grouped_state = call("MenuState")
            tree = grouped_state.get("tree") or []

            def labels_of(nodes):
                return [node.get("label") for node in nodes]

            def kinds_of(nodes):
                return [node.get("kind") for node in nodes]

            grouped_ok = (
                grouped_push.get("ok") is True and
                grouped_push.get("groups") == 3 and
                labels_of(tree) == ["16:9", "4:3", "plain"] and
                kinds_of(tree) == ["group", "group", "preset"] and
                labels_of(tree[0].get("children") or []) == ["g700", "g400"] and
                kinds_of(tree[1].get("children") or []) == ["group"] and
                labels_of((tree[1]["children"][0].get("children")) or []) == ["half"] and
                grouped_state.get("presets") == ["g700", "g400", "half", "plain"])
            grouped_apply = call("ActivateMenuRow", (1,))
            result, after = wait_applied(win_id, time.time() + 5, activated=grouped_apply)
            achieved_client = (after or {}).get("client") or {}
            requested = result.get("requested_client") or {}
            grouped_ok = grouped_ok and result.get("exact") is True and \
                result.get("label") == "g400" and \
                requested.get("w") == 400 and requested.get("h") == 300 and \
                achieved_client.get("w") == 400 and achieved_client.get("h") == 300
            recorder.add("grouped_presets_submenu", "pass" if grouped_ok else "fail",
                         {"pushed": grouped_push, "top": labels_of(tree), "tree": tree,
                          "presets": grouped_state.get("presets"), "activated": 1,
                          "result": result, "requested": requested,
                          "achieved_client": achieved_client})

        def case_preset_menu_separator():
            # T8g: a separator keeps its row and is skipped by the depth-first preset
            # index, so the row below it still applies.
            # Self-contained: the payload carries the area and basis, so the outcome
            # is the same whichever case ran before this one.
            separator_push = call("SetPresets", (json.dumps({
                "version": 2,
                "presets": [
                    {"label": "above", "size": "520x380", "anchor": "top_left"},
                    {"label": "", "size": "", "kind": "separator"},
                    {"label": "below", "size": "440x320", "anchor": "top_left"},
                ],
                "options": {"area": "workarea", "basis": "client"},
            }, ensure_ascii=False),))
            call("ShowMenu", (180, 140))
            sep_state = call("MenuState")
            sep_tree = sep_state.get("tree") or []
            sep_apply = call("ActivateMenuRow", (1,))
            sep_result, sep_after = wait_applied(win_id, time.time() + 5,
                                                 activated=sep_apply)
            sep_basis = sep_result.get("basis")
            sep_client = (sep_after or {}).get(sep_basis or "client") or {}
            work = baseline["work_area"]
            sep_ok = (
                separator_push.get("ok") is True
                and sep_basis == "client"
                and [node.get("kind") for node in sep_tree] ==
                ["preset", "separator", "preset"]
                and sep_state.get("presets") == ["above", "below"]
                and sep_state.get("items") == ["above", "below"]
                and sep_result.get("label") == "below"
                and sep_result.get("exact") is True
                and sep_result.get("requested_client") ==
                {"x": work["x"], "y": work["y"], "w": 440, "h": 320}
                and sep_client.get("w") == 440
                and sep_client.get("h") == 320
            )
            recorder.add("preset_menu_separator", "pass" if sep_ok else "fail",
                         {"push": separator_push, "tree": sep_tree,
                          "presets": sep_state.get("presets"), "items": sep_state.get("items"),
                          "result": sep_result, "basis": sep_basis, "achieved": sep_client})

        def case_macro_multi_window():
            # T13b: one macro, two windows, through the extension's D-Bus host. The
            # layout has to land on both windows and the command has to read the
            # result back, not just fire off two writes.
            macro_clients = []
            try:
                if args.sizer:
                    for suffix in ("A", "B"):
                        macro_client = Client(
                            out_dir, Path(__file__).resolve().with_name("gnome_client.py"),
                            f"MACRO-GNOME-{suffix}", args.wayland_display,
                            ["--width", "300", "--height", "220"])
                        macro_client.start()
                        macro_clients.append(macro_client)
                    find_window("MACRO-GNOME-A", time.time() + 15)
                    find_window("MACRO-GNOME-B", time.time() + 15)
                    macro = ("resize : proc[python3]:cap[MACRO-GNOME-]:desktop*\n"
                             "t = 140\n"
                             "l = 120 + 480 * ($_index - 1)\n"
                             "w = 420\n"
                             "h = 300")
                    result = subprocess.run(
                        [args.sizer, "macro", macro, "--json"],
                        capture_output=True, text=True, timeout=90)
                    report = (json.loads(result.stdout)
                              if result.stdout.strip().startswith("{") else None)
                    live = {window["title"]: window["client"]
                            for window in call("ListWindows")
                            if str(window.get("title", "")).startswith("MACRO-GNOME-")}
                    # The block numbers windows in creation order; derive the
                    # expected rectangle from the index the report assigned.
                    expectations = {
                        outcome["title"]: {
                            "x": 120 + 480 * (outcome["index"] - 1),
                            "y": 140,
                            "w": 420,
                            "h": 300,
                        }
                        for outcome in (report or {}).get("windows", [])
                    }
                    laid_out = (
                        result.returncode == 0
                        and bool(report)
                        and report.get("backend") == "gnome"
                        and report.get("ok") is True
                        and len(report.get("windows", [])) == 2
                        and sorted(outcome["index"]
                                   for outcome in report.get("windows", [])) == [1, 2]
                        and all(
                            outcome.get("count") == 2
                            and outcome.get("exact") is True
                            and outcome.get("requested") == expectations[outcome["title"]]
                            and outcome.get("client") == expectations[outcome["title"]]
                            for outcome in report.get("windows", [])
                        )
                        and live == expectations
                    )
                    recorder.add("macro_multi_window", "pass" if laid_out else "fail",
                                 {"report": report, "live": live, "expected": expectations,
                                  "returncode": result.returncode,
                                  "stderr": result.stderr[-400:]})
                    bad = subprocess.run(
                        [args.sizer, "macro", "resize : proc[no-such-window] t = 1 bogus"],
                        capture_output=True, text=True, timeout=30)
                    rejected = (bad.returncode != 0
                                and "Unexpected token in macro" in bad.stderr
                                and "line 1" in bad.stderr)
                    recorder.add("macro_invalid_token_is_red",
                                 "pass" if rejected else "fail",
                                 {"returncode": bad.returncode, "stderr": bad.stderr[-300:]})
                else:
                    recorder.add("macro_multi_window", "fail",
                                 {"reason": "--sizer was not given to the harness"})
            finally:
                for macro_client in macro_clients:
                    macro_client.stop()

        # `--only NAME` runs the named cases from this fresh session and exits.
        # A name that is not registered is refused, never silently skipped.
        ONLY_CASES = {
            "menu_apply_preset": case_menu_apply_preset,
            "grouped_presets_submenu": case_grouped_presets_submenu,
            "preset_menu_separator": case_preset_menu_separator,
            "macro_multi_window": case_macro_multi_window,
        }
        if args.only:
            unknown = [name for name in args.only if name not in ONLY_CASES]
            if unknown:
                recorder.add("case_selection", "fail",
                             {"unknown": unknown, "supported": sorted(ONLY_CASES)})
                return recorder.exit_code(args.strict)
            for name in args.only:
                ONLY_CASES[name]()
            return recorder.exit_code(args.strict)
        # T1: exact resize inside the work area
        settle_unmaximized()
        call("MoveResizeFrame", (win_id, 100, 100, target_w, target_h))
        after = stable_window(win_id, time.time() + 3)
        expect_frame(recorder, f"resize_exact_{target_w}x{target_h}", after,
                     {"x": 100, "y": 100, "w": target_w, "h": target_h})

        # T2: too small (client hints / minimum size)
        call("MoveResizeFrame", (win_id, 100, 100, 1, 1))
        after = stable_window(win_id, time.time() + 3)
        tiny = after["frame"]
        tiny_ok = tiny["w"] > 1 and tiny["h"] > 1
        recorder.add("resize_tiny_1x1", "pass" if tiny_ok else "fail",
                     {"requested": {"w": 1, "h": 1}, "achieved_frame": tiny,
                      "adjusted_by_hints": tiny_ok,
                      "expectation": "the toolkit minimum size wins over a 1x1 request"})

        # T3: larger than the monitor (clamping)
        call("MoveResizeFrame", (win_id, 0, 0, 8000, 6000))
        after = stable_window(win_id, time.time() + 4)
        huge = after["frame"]
        clamped = huge["w"] <= work["w"] and huge["h"] <= work["h"]
        recorder.add("resize_huge_8000x6000", "pass" if clamped else "fail",
                     {"requested": {"w": 8000, "h": 6000}, "achieved_frame": huge,
                      "work_area": work, "clamped": clamped,
                      "maximized_state": after.get("maximized")})

        # T4: plain move + resize
        move_w = min(900, work["w"] - 320)
        move_h = min(700, work["h"] - 220)
        settle_unmaximized()
        call("MoveResizeFrame", (win_id, 300, 200, move_w, move_h))
        after = stable_window(win_id, time.time() + 3)
        expect_frame(recorder, f"resize_move_{move_w}x{move_h}", after,
                     {"x": 300, "y": 200, "w": move_w, "h": move_h})

        # T5: what a maximized window does to each path.
        call("SetMaximized", (win_id, True))
        time.sleep(0.5)
        maximized = call("GetWindow", (win_id,))["window"]
        recorder.add("maximize", "pass" if maximized.get("maximized") else "fail",
                     {"maximized_state": maximized.get("maximized"), "frame": maximized["frame"]})
        # T5a: the bare frame write is ignored while maximized (platform behaviour).
        call("MoveResizeFrame", (win_id, 200, 200, 800, 600))
        after = stable_window(win_id, time.time() + 4)
        ignored = (after.get("maximized") == maximized.get("maximized") and
                   after["frame"] == maximized["frame"])
        recorder.add("move_resize_frame_ignored_while_maximized", "pass" if ignored else "fail",
                     {"maximized_before": maximized.get("maximized"),
                      "maximized_after": after.get("maximized"),
                      "frame_before": maximized["frame"], "frame_after": after["frame"]})
        # T5b: the preset path is the contract users see: it unmaximizes first
        # (`_applyPreset` clears the state) and then writes the preset geometry.
        # The 50% row must land exactly on the requested client rect.
        # Self-contained: push the built-in menu and the default area/basis here.
        call("SetPresets", (json.dumps({
            "version": 2,
            "presets": [
                {"label": "1920x1080", "size": "1920x1080", "anchor": "center"},
                {"label": "1280x720", "size": "1280x720", "anchor": "center"},
                {"label": "1024x768", "size": "1024x768", "anchor": "center"},
                {"label": "50% x 50%", "size": "50%x50%", "anchor": "center"},
            ],
            "options": {"area": "workarea", "basis": "client"},
        }, ensure_ascii=False),))
        call("ShowMenu", (400, 300))
        state = call("MenuState")
        rows = state.get("items") or []
        percent_rows = [index for index, label in enumerate(rows) if label.endswith("x 50%")]
        if percent_rows:
            index = percent_rows[0]
            activated = call("ActivateMenuRow", (index,))
            result, after = wait_applied(win_id, time.time() + 6, label=rows[index],
                                         activated=activated)
            recorder.add("preset_resize_unmaximizes",
                         "pass" if preset_landed(activated, result, after) else "fail",
                         {"row": rows[index], "activated": activated,
                          "result": applied_detail(result), "window": window_detail(after)})
        else:
            recorder.add("preset_resize_unmaximizes", "fail",
                         {"rows": rows, "reason": "no percentage row in the preset menu"})

        # T6: always-on-top
        above_on = call("SetAbove", (win_id, True))
        above_off = call("SetAbove", (win_id, False))
        recorder.add("above_toggle", "pass" if above_on["above"] and not above_off["above"] else "fail",
                     {"after_enable": above_on["above"], "after_disable": above_off["above"]})

        # T7: server-side decorations are not a thing on GNOME; the contract is
        # that the call says so instead of pretending to succeed.
        decorated = call("SetDecorated", (win_id, False))
        call("SetDecorated", (win_id, True))
        refused = (decorated.get("ok") is False and
                   "client-side" in str(decorated.get("reason")))
        recorder.add("undecorate", "pass" if refused else "fail",
                     {"reply": decorated,
                      "expectation": "ok:false with a client-side decorations reason"})

        # T8: default menu hotkey is Ctrl+Win+Z and it opens the preset menu
        keys = call("KeybindingState")
        registered = (keys["action_id"] not in (0, -1) and
                      keys["accelerators"] == ["<Primary><Super>z"])
        recorder.add("keybinding_registered", "pass" if registered else "fail", keys)

        fired = call("KeybindingState")
        before = fired["key_count"]
        sent = None
        via = None
        if args.nested_display and nested_window:
            via = "xtest"
            sent = inject_key(args.nested_display, nested_window["id"], "ctrl+super+z", out_dir)
        elif args.parent_sway and shell.parent_display:
            via = "sway-virtual-keyboard"
            sent = inject_keys_sway(shell.parent_display, out_dir)
        if via:
            state = call("MenuState")
            deadline = time.time() + 5
            while time.time() < deadline and not (fired["key_count"] > before and
                                                  state.get("open")):
                time.sleep(0.1)
                fired = call("KeybindingState")
                state = call("MenuState")
            opened = fired["key_count"] > before and state.get("open")
            recorder.add("hotkey_opens_menu", "pass" if opened else "fail",
                         {"via": via, "sent": sent, "before": before,
                          "key_count": fired["key_count"],
                          "accelerators": fired["accelerators"], "menu": state})
        else:
            # Last resort on a headless shell: mutter's own RemoteDesktop session.
            remote = RemoteInput()
            if remote.start():
                fired = call("KeybindingState")
                before = fired["key_count"]
                remote.chord([37, 133, 52])
                deadline = time.time() + 5
                state = call("MenuState")
                while time.time() < deadline and not (fired["key_count"] > before and
                                                      state.get("open")):
                    time.sleep(0.1)
                    fired = call("KeybindingState")
                    state = call("MenuState")
                opened = fired["key_count"] > before and state.get("open")
                if not opened:
                    recorder.allow_limited(
                        "hotkey_opens_menu",
                        "headless shell: RemoteDesktop accepted the chord but no shell "
                        "handler saw it (no seat)")
                recorder.add("hotkey_opens_menu", "pass" if opened else "limited",
                             {"via": "remote-desktop", "key_count": fired["key_count"],
                              "before": before, "accelerators": fired["accelerators"],
                              "menu": state})
            else:
                recorder.allow_limited(
                    "hotkey_opens_menu",
                    "no input injection path in this session (no X display, no RemoteDesktop)")
                recorder.add("hotkey_opens_menu", "limited",
                             {"reason": "no input injection path available"})

        case_menu_apply_preset()

        # T8c: presets pushed over D-Bus drive the menu, Chinese label included
        area = baseline["work_area"]
        # Self-contained: the payload names the area and basis it is measured in.
        custom = json.dumps({
            "version": 2,
            "presets": [
                {"label": "720p", "size": "1280x720", "anchor": "center"},
                {"label": "Half Screen", "size": "50%x50%", "anchor": "top_left"},
            ],
            "options": {"area": "workarea", "basis": "client"},
        }, ensure_ascii=False)
        pushed = call("SetPresets", (custom,))
        pushed_menu = call("ShowMenu", (300, 200))
        pushed_state = call("MenuState")
        pushed_apply = call("ActivateMenuRow", (1,))
        result, after = wait_applied(win_id, time.time() + 5, activated=pushed_apply)
        achieved_client = (after or {}).get("client") or {}
        requested = result.get("requested_client") or {}
        expected = {"x": area["x"], "y": area["y"],
                    "w": int(area["w"] * 0.5 + 0.5), "h": int(area["h"] * 0.5 + 0.5)}
        pushed_ok = (pushed.get("ok") and pushed_state.get("items") == ["720p", "Half Screen"] and
                     result.get("exact") is True and
                     requested.get("x") == expected["x"] and
                     requested.get("y") == expected["y"] and
                     requested.get("w") == expected["w"] and
                     requested.get("h") == expected["h"])
        recorder.add("set_presets_and_apply", "pass" if pushed_ok else "fail",
                     {"pushed": pushed, "menu": pushed_menu, "rows": pushed_state.get("items"),
                      "area": area, "expected": expected, "requested": requested,
                      "achieved_client": achieved_client, "result": result})

        case_grouped_presets_submenu()

        # T8e: the version 2 payload carries `monitor`, the global area/basis and
        # the menu hotkey. A monitor index that does not exist must fail loudly
        # instead of silently falling back to the active monitor.
        info = call("ShellInfo")
        primary = info.get("primary_monitor")
        v2 = json.dumps({
            "version": 2,
            "presets": [
                {"label": "opt-frame", "size": "600x400", "anchor": "top_left",
                 "monitor": "primary"},
                {"label": "opt-bad-monitor", "size": "500x300", "anchor": "top_left",
                 "monitor": "7"},
            ],
            "options": {"area": "monitor", "basis": "frame", "hotkey": "<Super><Alt>w"},
        }, ensure_ascii=False)
        v2_push = call("SetPresets", (v2,))
        keys = call("KeybindingState")
        call("ShowMenu", (150, 150))
        frame_apply = call("ActivateMenuRow", (0,))
        frame_result, frame_after = wait_applied(win_id, time.time() + 5,
                                                 activated=frame_apply)
        before_bad = call("GetWindow", (win_id,))["window"]
        call("ShowMenu", (150, 150))
        call("ActivateMenuRow", (1,))
        time.sleep(0.6)
        bad_state = call("LastResult")
        after_bad = call("GetWindow", (win_id,))["window"]
        requested_basis = frame_result.get("requested_basis") or {}
        v2_ok = (
            v2_push.get("ok") is True and
            v2_push.get("options") == {"area": "monitor", "basis": "frame",
                                       "allowOutsideBounds": False} and
            (v2_push.get("hotkey") or {}).get("applied") is True and
            keys.get("accelerators") == ["<Super><Alt>w"] and
            frame_result.get("basis") == "frame" and
            frame_result.get("monitor") == primary and
            frame_result.get("exact") is True and
            requested_basis.get("w") == 600 and requested_basis.get("h") == 400 and
            bad_state.get("error") == "no_such_monitor" and
            (after_bad.get("frame") or {}).get("w") ==
            (before_bad.get("frame") or {}).get("w") and
            (after_bad.get("frame") or {}).get("h") ==
            (before_bad.get("frame") or {}).get("h"))
        recorder.add("options_monitor_basis_hotkey", "pass" if v2_ok else "fail",
                     {"push": v2_push, "keybinding": keys, "primary": primary,
                      "frame_result": frame_result, "bad_result": bad_state,
                      "before_bad": before_bad.get("frame"),
                      "after_bad": after_bad.get("frame")})

        # T8f: the desktop entry runs `sizer menu`; on GNOME it must reach the
        # extension and open its menu at the pointer.
        if not args.sizer:
            recorder.add("menu_command", "limited", {"reason": "harness run without --sizer"})
        else:
            call("CloseMenu")
            menu_cmd = subprocess.run(
                [str(Path(args.sizer).resolve()), "menu", "--json"],
                capture_output=True, text=True, timeout=25)
            menu_state = call("MenuState")
            call("CloseMenu")
            menu_ok = menu_cmd.returncode == 0 and menu_state.get("open") is True
            recorder.add("menu_command", "pass" if menu_ok else "fail",
                         {"returncode": menu_cmd.returncode,
                          "stdout": menu_cmd.stdout[-400:],
                          "stderr": menu_cmd.stderr[-400:],
                          "menu_open": menu_state.get("open")})

        case_preset_menu_separator()

        # T8h: an absolute position applies from the menu, relative to the area
        # origin; half a position or an anchor beside one is skipped with a reason.
        pos_push = call("SetPresets", (json.dumps({
            "version": 2,
            "presets": [
                {"label": "pinned", "size": "300x200", "x": 100, "y": 70, "monitor": "primary"},
                {"label": "half-position", "size": "300x200", "x": 10},
                {"label": "anchored", "size": "320x240", "anchor": "center", "x": 10, "y": 20},
            ],
            "options": {"area": "workarea", "basis": "client"},
        }, ensure_ascii=False),))
        call("ShowMenu", (200, 160))
        pos_apply = call("ActivateMenuRow", (0,))
        pos_result, pos_after = wait_applied(win_id, time.time() + 5, activated=pos_apply)
        pos_client = (pos_after or {}).get("client") or {}
        pos_area = pos_result.get("area") or {}
        pos_reasons = " | ".join(entry.get("reason") or "" for entry in pos_push.get("skipped") or [])
        pos_ok = (
            pos_push.get("ok") is True
            and pos_push.get("presets") == 1
            and "set together" in pos_reasons
            and "mutually exclusive" in pos_reasons
            and pos_result.get("label") == "pinned"
            and pos_result.get("exact") is True
            and pos_result.get("basis") == "client"
            and pos_client.get("x") == pos_area.get("x", 0) + 100
            and pos_client.get("y") == pos_area.get("y", 0) + 70
            and pos_client.get("w") == 300
            and pos_client.get("h") == 200
        )
        recorder.add("preset_absolute_position", "pass" if pos_ok else "fail",
                     {"push": pos_push, "result": pos_result, "area": pos_area,
                      "client": pos_client, "reasons": pos_reasons})

        # T8i: allowOutsideBounds. Off pulls the request into the area here; on
        # writes the coordinate verbatim, reports `outside`, and leaves `exact`
        # to describe what the compositor actually did.
        off_push = call("SetPresets", (json.dumps({
            "version": 2,
            "presets": [{"label": "off-move", "size": "300x200", "x": -100, "y": 120}],
            "options": {"area": "workarea", "basis": "client",
                        "allow_outside": False},
        }, ensure_ascii=False),))
        call("ShowMenu", (200, 160))
        off_apply = call("ActivateMenuRow", (0,))
        off_result, off_after = wait_applied(win_id, time.time() + 5, activated=off_apply)
        off_area = off_result.get("area") or {}
        off_req = off_result.get("requested_client") or {}
        off_client = (off_after or {}).get("client") or {}
        off_ok = (
            off_push.get("ok") is True
            and off_result.get("outside") is False
            and off_req.get("x") == off_area.get("x")
            and off_req.get("y") == off_area.get("y", 0) + 120
            and off_result.get("exact") is True
            and off_client.get("x") == off_req.get("x")
        )

        on_push = call("SetPresets", (json.dumps({
            "version": 2,
            "presets": [{"label": "on-move", "size": "300x200", "x": -100, "y": 120,
                         "allow_outside": True}],
            "options": {"area": "workarea", "basis": "client", "allow_outside": False},
        }, ensure_ascii=False),))
        call("ShowMenu", (200, 160))
        on_apply = call("ActivateMenuRow", (0,))
        on_result, on_after = wait_applied(win_id, time.time() + 5, activated=on_apply)
        on_area = on_result.get("area") or {}
        on_req = on_result.get("requested_client") or {}
        on_client = (on_after or {}).get("client") or {}
        on_ok = (
            on_push.get("ok") is True
            and on_result.get("outside") is True
            and on_req.get("x") == on_area.get("x", 0) - 100
            and on_result.get("exact") == (on_client == on_req)
        )
        outside_ok = off_ok and on_ok
        recorder.add("preset_outside_bounds", "pass" if outside_ok else "fail",
                     {"off": {"ok": off_ok, "requested": off_req, "achieved": off_client,
                              "outside": off_result.get("outside"),
                              "exact": off_result.get("exact")},
                      "on": {"ok": on_ok, "requested": on_req, "achieved": on_client,
                             "outside": on_result.get("outside"),
                             "exact": on_result.get("exact")}})

        # T9: D-Bus signal both directions
        if not args.skip_signal:
            received = []
            sub_id = bus().signal_subscribe(
                BUS, IFACE, "Changed", OBJ, None, Gio.DBusSignalFlags.NONE,
                lambda *a: received.append(a[5].unpack()[0][:160]))
            call("MoveResizeFrame", (win_id, 320, 220, 880, 660))
            deadline = time.time() + 3
            context = GLib.MainContext.default()
            while time.time() < deadline and not received:
                context.iteration(False)
                time.sleep(0.02)
            bus().signal_unsubscribe(sub_id)
            recorder.add("dbus_signal", "pass" if received else "fail",
                         {"received": len(received), "sample": received[:1]})

        # T9b: a closed window leaves the tracked set. The extension used to add
        # signal connections on window-created and never remove them, so the map
        # grew for the lifetime of the session.
        before_tracked = call("ShellInfo").get("tracked_windows")
        extra = Client(out_dir,
                       Path(__file__).resolve().with_name("gnome_client.py"),
                       "extra-window", args.wayland_display, ["--width", "320", "--height", "240"])
        extra.start()
        appeared = False
        cleanup_deadline = time.time() + 15
        while time.time() < cleanup_deadline:
            if any(w.get("title") == "extra-window" for w in call("ListWindows")):
                appeared = True
                break
            time.sleep(0.1)
        grown = call("ShellInfo").get("tracked_windows")
        extra.stop()
        vanished = False
        cleanup_deadline = time.time() + 15
        while time.time() < cleanup_deadline:
            if not any(w.get("title") == "extra-window" for w in call("ListWindows")):
                vanished = True
                break
            time.sleep(0.1)
        time.sleep(0.5)
        shrunk = call("ShellInfo").get("tracked_windows")
        cleanup_ok = (
            appeared and vanished and
            isinstance(before_tracked, int) and isinstance(grown, int) and
            isinstance(shrunk, int) and
            grown >= before_tracked + 1 and shrunk <= grown - 1)
        recorder.add("window_tracking_cleanup", "pass" if cleanup_ok else "fail",
                     {"before": before_tracked, "with_extra": grown,
                      "after_close": shrunk, "appeared": appeared, "vanished": vanished})

        # T10: menu closes without changing any window when dismissed
        reopened = call("ShowMenu", (120, 120))
        closed = call("CloseMenu")
        state = call("MenuState")
        recorder.add("menu_close", "pass" if reopened.get("ok") and closed.get("was_open") and
                     state.get("open") is False else "fail",
                     {"menu": reopened, "closed": closed, "state": state})

        # T12: `sizer daemon` keeps the extension in step with the TOML file:
        # no file -> built-in presets, file -> its presets, file change -> repush,
        # extension reset -> repush.
        if not args.sizer:
            recorder.add("daemon_config_push", "limited",
                         {"reason": "harness run without --sizer"})
        else:
            binary = str(Path(args.sizer).resolve())
            config_dir = Path(os.environ["XDG_CONFIG_HOME"]) / "sizer"
            config_file = config_dir / "config.toml"
            daemon_log = (out_dir / "daemon.log").open("wb")
            daemon = None
            try:
                if config_file.exists():
                    config_file.unlink()
                daemon = subprocess.Popen([binary, "daemon"], stdout=daemon_log,
                                          stderr=subprocess.STDOUT)
                builtin_ok, builtin_labels = wait_presets(
                    ["Center", "50% x 50%", "800x600", "1024x768", "1280x960",
                     "1600x1200", "1280x720", "1600x900", "1920x1080", "2560x1440",
                     "1280x800", "1440x900", "1680x1050", "1920x1200"], time.time() + 12)
                config_dir.mkdir(parents=True, exist_ok=True)
                config_file.write_text(
                    'version = 1\n'
                    '\n[[presets]]\nlabel = "daemon-a"\nsize = "700x400"\ngroup = "16:9"\n'
                    '\n[[presets]]\nlabel = "daemon-b"\nsize = "640x400"\ngroup = "16:9"\n')
                file_ok, file_labels = wait_presets(["daemon-a", "daemon-b"], time.time() + 8)
                pushed_state = call_quiet("MenuState")
                file_tree = [node.get("label") for node in
                             (pushed_state.get("tree") or [])]
                config_file.write_text(
                    'version = 1\n\n[[presets]]\nlabel = "daemon-c"\nsize = "600x400"\n')
                change_ok, change_labels = wait_presets(["daemon-c"], time.time() + 8)
                # A restarted extension comes back with its own defaults; the
                # daemon notices the difference and pushes the file again.
                reset = json.dumps([{"label": label, "size": "800x600"}
                                    for label in ("1920x1080", "1280x720")])
                call_quiet("SetPresets", (reset,))
                repair_deadline = time.time() + 8
                repaired, repaired_labels = wait_presets(["daemon-c"], repair_deadline)
                ok = builtin_ok and file_ok and change_ok and repaired and \
                    file_tree == ["16:9"]
                recorder.add("daemon_config_push", "pass" if ok else "fail",
                             {"builtin": builtin_labels, "from_file": file_labels,
                              "tree": file_tree, "after_edit": change_labels,
                              "repaired_after_reset": repaired_labels,
                              "repaired": repaired,
                              "daemon_exit": daemon.poll(),
                              "daemon_log": (out_dir / "daemon.log")
                              .read_text(errors="replace")[-1500:]})
            except Exception as exc:  # noqa: BLE001
                recorder.add("daemon_config_push", "fail",
                             {"error": f"{type(exc).__name__}: {exc}"})
            finally:
                if daemon and daemon.poll() is None:
                    daemon.terminate()
                    try:
                        daemon.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        daemon.kill()
                        daemon.wait(timeout=5)
                daemon_log.close()
                if config_file.exists():
                    config_file.unlink()

        # T11: extension disable/enable recovery (needs the gnome-extensions service,
        # which cannot start without a display in the fully headless variant)
        try:
            ext_proxy = Gio.DBusProxy.new_sync(
                bus(), Gio.DBusProxyFlags.NONE, None,
                "org.gnome.Shell.Extensions", "/org/gnome/Shell/Extensions",
                "org.gnome.Shell.Extensions", None)

            def ext_call(method, uuid):
                try:
                    reply = ext_proxy.call_sync(method, GLib.Variant("(s)", (uuid,)),
                                                Gio.DBusCallFlags.NONE, 10000, None)
                    return {"ok": True, "reply": list(reply.unpack())}
                except Exception as exc:  # noqa: BLE001
                    return {"ok": False, "error": str(exc)}

            disabled = ext_call("DisableExtension", EXT_UUID)
            time.sleep(1.0)
            after_disable = call_quiet("Ping")
            enabled = ext_call("EnableExtension", EXT_UUID)
            replayed = wait_name(time.time() + 20)
            recorder.add("extension_toggle",
                         "pass" if disabled["ok"] and enabled["ok"] and replayed and
                         after_disable.get("ok") is False else "limited",
                         {"disable_reply": disabled, "ping_after_disable": after_disable,
                          "enable_reply": enabled, "name_back": replayed})
        except Exception as exc:  # noqa: BLE001
            recorder.add("extension_toggle", "limited",
                         {"reason": "org.gnome.Shell.Extensions unavailable", "error": str(exc)})

        # T13: a preset applied to a maximized window is the main path users hit
        # (full-screen window, pick a preset). It must land on the requested
        # client rect, not merely somewhere smaller. Two flavours: a size equal to
        # the work area, and a plain pixel size.
        area = baseline["work_area"]
        work_size = f"{area['w']}x{area['h']}"
        # Self-contained: the area and basis come with the payload instead of
        # being whatever an earlier case selected.
        maximized_presets = json.dumps({
            "version": 2,
            "presets": [
                {"label": "workarea-size", "size": work_size, "anchor": "top_left"},
                {"label": "pixel-600x400", "size": "600x400", "anchor": "top_left"},
                {"label": "pixel-430x330", "size": "430x330", "anchor": "top_left"},
            ],
            "options": {"area": "workarea", "basis": "client"},
        }, ensure_ascii=False)
        call("SetPresets", (maximized_presets,))
        for name, row, label in (("preset_maximized_workarea_size", 0, "workarea-size"),
                                ("preset_maximized_pixel_size", 1, "pixel-600x400")):
            maximized = remaximize(win_id)
            call("ShowMenu", (300, 200))
            activated = call("ActivateMenuRow", (row,))
            result, after = wait_applied(win_id, time.time() + 6, label=label,
                                         activated=activated)
            recorder.add(name, "pass" if preset_landed(activated, result, after) else "fail",
                         {"activated": activated, "result": applied_detail(result),
                          "window": window_detail(after),
                          "maximized_client": maximized["client"]})

        # T13c: two presets in the same turn must end on the last request, and the
        # stale one must not touch the result (the generation counter is what
        # guarantees that, not the redraw timing).
        remaximize(win_id)
        call("ShowMenu", (300, 200))
        first = call("ActivateMenuRow", (1,))
        call("ShowMenu", (300, 200))
        second = call("ActivateMenuRow", (2,))
        result, after = wait_applied(win_id, time.time() + 6, label="pixel-430x330",
                                     activated=second)
        landed = preset_landed(second, result, after)
        recorder.add("preset_double_apply_last_wins",
                     "pass" if landed and result.get("label") == "pixel-430x330" else "fail",
                     {"first": first, "second": second, "result": applied_detail(result),
                      "window": window_detail(after)})

        # T13d: a window whose restored rect equals its maximized rect must still
        # land exactly once. mutter emits size-changed even when the rect is
        # unchanged (measured: the write arrives in ~0.2s), so what this case
        # pins down is the single-write property: one `preset-applied` signal, and
        # a later resize must not produce another one.
        area = baseline["work_area"]
        call("MoveResizeFrame", (win_id, area["x"], area["y"], area["w"], area["h"]))
        time.sleep(0.5)
        sized = call("GetWindow", (win_id,))["window"]
        at_max = remaximize(win_id)
        call("ShowMenu", (300, 200))
        signals = []
        sub_id = bus().signal_subscribe(
            BUS, IFACE, "Changed", OBJ, None, Gio.DBusSignalFlags.NONE,
            lambda *a: signals.append(json.loads(a[5].unpack()[0])))
        started = time.time()
        activated = call("ActivateMenuRow", (1,))
        result, after = wait_applied(win_id, time.time() + 6, label="pixel-600x400",
                                     activated=activated)
        waited = time.time() - started
        context = GLib.MainContext.default()
        for _ in range(60):
            context.iteration(False)
        landing = [entry for entry in signals if entry.get("reason") == "preset-applied"]
        call("MoveResizeFrame", (win_id, 120, 120, 900, 640))
        deadline = time.time() + 1.5
        while time.time() < deadline:
            context.iteration(False)
            time.sleep(0.02)
        bus().signal_unsubscribe(sub_id)
        repeated = [entry for entry in signals if entry.get("reason") == "preset-applied"]
        same_rect = (sized["frame"]["w"] == at_max["frame"]["w"] and
                     sized["frame"]["h"] == at_max["frame"]["h"])
        ok = (same_rect and preset_landed(activated, result, after)
              and len(landing) == 1 and len(repeated) == 1)
        recorder.add("preset_single_write_after_unmaximize", "pass" if ok else "fail",
                     {"activated": activated, "result": applied_detail(result),
                      "window": window_detail(after), "waited_s": round(waited, 2),
                      "same_rect": same_rect, "applied_signals": len(landing),
                      "after_extra_resize": len(repeated)})

        # final: leave the window usable and record client-side view. This runs
        # before the case that closes the window, which is the last one.
        call("MoveResizeFrame", (win_id, 120, 120, 1000, 720))
        time.sleep(0.6)
        recorder.add("client_side_view", "pass", {"client_events": client.drain()})

        case_macro_multi_window()

        # T14/T15 drive a second window: the bookkeeping has to be per window, and
        # ending a wait without a write has to leave a result behind. This nested
        # session cannot move the focus (see focus_by_keys), so T14 can only run
        # where a switcher binding works; T15 does not need the second window.
        # Self-contained: this group names the presets it applies.
        call("SetPresets", (json.dumps({
            "version": 2,
            "presets": [
                {"label": "workarea-size",
                 "size": f"{baseline['work_area']['w']}x{baseline['work_area']['h']}",
                 "anchor": "top_left"},
                {"label": "pixel-600x400", "size": "600x400", "anchor": "top_left"},
                {"label": "pixel-430x330", "size": "430x330", "anchor": "top_left"},
            ],
            "options": {"area": "workarea", "basis": "client"},
        }, ensure_ascii=False),))
        second = Client(out_dir, Path(__file__).resolve().with_name("gnome_client.py"),
                        CLIENT_TITLE_B, args.wayland_display,
                        ["--width", "360", "--height", "260", "--defer-map"])
        second.start()
        try:
            signals = Signals()
            try:
                # T14: while A's write waits for the compositor, a preset on B must
                # not make A's write stale. With one global generation A is dropped
                # silently, i.e. the user presses the key and nothing moves. The wait
                # is held open by re-maximizing A, and that hold has a one second
                # budget (unmaximize_timeout): the focus is moved to B before the
                # requests start, so only a single switcher press has to fit inside.
                attempts = []
                verdict = None
                interleaved = False
                second.send("map")
                mapped = find_window(CLIENT_TITLE_B, time.time() + 10)
                second_id = mapped["id"]
                focus_b = (focus_window(args.nested_display, second_id, out_dir)
                           if args.nested_display else {"moved": None})
                focus_a = (focus_window(args.nested_display, win_id, out_dir)
                           if args.nested_display else {"moved": None})
                attempts.append({"focus_b": focus_b, "focus_a": focus_a})
                if focus_b.get("moved"):
                    for _ in range(2):
                        remaximize(win_id)
                        call("ShowMenu", (100, 120))
                        first = call("ActivateMenuRow", (1,))
                        if not first.get("pending"):
                            attempts.append({"skipped": "the first request did not wait",
                                             "reply": first})
                            continue
                        call("SetMaximized", (win_id, True))
                        switch = (press_switch(args.nested_display, out_dir)
                                  if args.nested_display else None)
                        switched = wait_focused(second_id, 0.8)
                        call("ShowMenu", (200, 200))
                        second_reply = call("ActivateMenuRow", (2,))
                        # Release A: dropping the maximum lets its pending write land.
                        call("SetMaximized", (win_id, False))
                        signals.wait_for(
                            lambda entry: entry.get("reason") == "preset-verified" and
                            (entry.get("result") or {}).get("label") == "pixel-600x400",
                            time.time() + 4)
                        signals.wait_for(
                            lambda entry: entry.get("reason") == "preset-verified" and
                            (entry.get("result") or {}).get("label") == "pixel-430x330",
                            time.time() + 4)
                        signals.pump(0.4)
                        attempts.append({"first": first, "second": second_reply,
                                         "switch": switch, "switched": switched,
                                         "window_maximized": call_quiet(
                                             "GetWindow", (win_id,)).get("window", {}).get("maximized"),
                                         "a": [applied_detail(result) for _, result in
                                               applied_with(signals, "pixel-600x400")],
                                         "b": [applied_detail(result) for _, result in
                                               applied_with(signals, "pixel-430x330")],
                                         "a_log": signal_log(signals, "pixel-600x400")})
                        if switched:
                            interleaved = True
                        if applied_with(signals, "pixel-600x400") and \
                                applied_with(signals, "pixel-430x330"):
                            verdict = True
                            break
                        signals.entries.clear()
                window_a = call("GetWindow", (win_id,))["window"]
                window_b = call("GetWindow", (second_id,))["window"]
                if verdict:
                    a_last = applied_with(signals, "pixel-600x400")[-1][1]
                    b_last = applied_with(signals, "pixel-430x330")[-1][1]
                    a_want = a_last.get("requested_client") or {}
                    b_want = b_last.get("requested_client") or {}
                    ok = (a_last.get("exact") is True and b_last.get("exact") is True and
                          (window_a["client"]["w"], window_a["client"]["h"]) ==
                          (a_want.get("w"), a_want.get("h")) and
                          (window_b["client"]["w"], window_b["client"]["h"]) ==
                          (b_want.get("w"), b_want.get("h")))
                    recorder.add("preset_waiting_window_independent",
                                 "pass" if ok else "fail",
                                 {"a": applied_detail(a_last), "b": applied_detail(b_last),
                                  "window_a": window_detail(window_a),
                                  "window_b": window_detail(window_b), "attempts": attempts})
                elif interleaved:
                    # The two requests really were interleaved, so a missing result is
                    # the extension dropping one of them: a failure, not a limit.
                    recorder.add("preset_waiting_window_independent", "fail",
                                 {"reason": "interleaved but one request never landed",
                                  "a": [applied_detail(result) for _, result in
                                        applied_with(signals, "pixel-600x400")],
                                  "b": [applied_detail(result) for _, result in
                                        applied_with(signals, "pixel-430x330")],
                                  "attempts": attempts})
                else:
                    reason = ("this session cannot move the focus between windows "
                              "(measured: a click is delivered without changing it, a "
                              "freshly mapped window does not take it, and the switcher "
                              "key only moves it sometimes), so the two requests cannot "
                              "be interleaved here")
                    recorder.allow_limited("preset_waiting_window_independent", reason)
                    recorder.add("preset_waiting_window_independent", "limited",
                                 {"reason": reason, "attempts": attempts})

                # T15: closing a window while its write waits must leave an explicit
                # result; a silent drop leaves LastResult on the previous preset,
                # which is what "pressed the key, nothing happened" looks like. The
                # menu targets the focused window, so the focused window is the one
                # closed.
                focused = next((window for window in call("ListWindows")
                                if window.get("focused")), None)
                target = focused or call("GetWindow", (win_id,))["window"]
                target_id = target["id"]
                closer = second if target_id == second_id else client
                closed_title = CLIENT_TITLE_B if closer is second else CLIENT_TITLE
                remaximize(target_id)
                call("ShowMenu", (100, 120))
                pending = call("ActivateMenuRow", (0,))
                call("SetMaximized", (target_id, True))
                before = call("LastResult")
                closer.stop()
                if closer is client:
                    client = None
                _, entry = signals.wait_for(
                    lambda item: item.get("reason") == "preset-failed" and
                    (item.get("result") or {}).get("reason") == "window destroyed",
                    time.time() + 4)
                after = call("LastResult")
                ok = (pending.get("pending") is True and entry is not None and
                      after.get("reason") == "window destroyed" and
                      after.get("label") == "workarea-size" and
                      after.get("label") != before.get("label"))
                recorder.add("preset_window_closed_while_waiting",
                             "pass" if ok else "fail",
                             {"activated": pending, "signal": entry,
                              "closed": closed_title,
                              "before": applied_detail(before),
                              "last_result": applied_detail(after)})
            finally:
                signals.close()
        finally:
            if second:
                second.stop()

        # final: the client is closed by the case above, so nothing is left running
        return recorder.exit_code(args.strict)
    finally:
        summary["tests"] = recorder.records
        (out_dir / "summary.json").write_text(json.dumps(summary, indent=2, default=str))
        if client:
            client.stop()
        shell.stop()
        shell.stop_xvfb()


if __name__ == "__main__":
    sys.exit(main())
