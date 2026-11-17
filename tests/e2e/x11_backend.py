#!/usr/bin/env python3
"""Phase 1 end-to-end check for the X11 backend and the `sizer` CLI.

The display is a private Xvfb server (:90-:99) where gnome-shell runs as a plain
X11 window manager, i.e. the same mutter code path as an Ubuntu Xorg session.
The user's live :0 session is never touched; every child process is terminated
by explicit PID in `finally` and the harness writes its own PID file.
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

X_SOCKET_DIR = Path("/tmp/.X11-unix")
TITLE = "SIZER-CLIENT"
CONFIG_TEXT = """\
version = 1

[options]
area = "workarea"
basis = "client"
tray = false
hotkey = "<Super><Alt>z"

[[presets]]
label = "center-700"
size = "700x500"
anchor = "center"
hotkey = "<Super><Alt>F8"

[[presets]]
label = "small-640"
size = "640x480"
hotkey = "<Super><Alt>F7"

[[presets]]
label = "Chinese Preset 500x300"
size = "500x300"
"""

GROUPED_CONFIG_TEXT = """\
version = 1

[[presets]]
label = "wide-720"
size = "1280x720"
group = "16:9"

[[presets]]
label = "wide-480"
size = "854x480"
group = "16:9"

[[presets]]
label = "plain-500"
size = "500x300"
"""

SOLO_CONFIG_TEXT = """\
version = 1

[[presets]]
label = "solo-620"
size = "620x460"
"""

SEPARATOR_CONFIG_TEXT = """\
version = 1

[[presets]]
label = "above"
size = "700x500"

[[presets]]
label = ""
size = ""
kind = "separator"

[[presets]]
label = "below"
size = "620x460"
"""

POSITION_CONFIG_TEXT = """\
version = 1

[[presets]]
label = "pinned"
size = "500x400"
x = 120
y = 70
monitor = "primary"

[[presets]]
label = "center-700"
size = "700x500"
anchor = "center"

[[presets]]
label = "small-640"
size = "640x480"
"""

OUTSIDE_CONFIG_TEXT = """\
version = 1

[options]
allow_outside_bounds = false

[[presets]]
label = "clamped"
size = "500x400"
x = -120
y = 300

[[presets]]
label = "allowed"
size = "500x400"
x = -120
y = 300
allow_outside_bounds = true
"""

OUTSIDE_GLOBAL_CONFIG_TEXT = """\
version = 1

[options]
allow_outside_bounds = true

[[presets]]
label = "clamped"
size = "500x400"
x = -120
y = 300
"""


# LIMITED is reserved for cases this environment genuinely cannot exercise, and
# every entry must name the limitation. Anything else is a fail, so `--strict`
# (used by `make gate`) cannot be satisfied by a broken main path.
STRICT_ALLOWED = {
    "hotkey_delivery_with_wm": "mutter owns the devices in this session, so the injected "
                               "probe keystroke never reaches the grab (the same probe is "
                               "verified in hotkey_delivery_control_no_wm without a WM)",
}


def free_display():
    for number in range(90, 100):
        if not (X_SOCKET_DIR / f"X{number}").exists():
            return f":{number}"
    raise RuntimeError("no free X display in :90-:99")


def stop(process, timeout=5):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.kill(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def wait_display(display, deadline):
    path = X_SOCKET_DIR / f"X{display[1:]}"
    while time.time() < deadline:
        if path.exists():
            return True
        time.sleep(0.05)
    return False


class Harness:
    def __init__(self, args):
        self.args = args
        self.out = Path(args.out).resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        self.results = self.out / "results.jsonl"
        self.results.write_text("")
        self.pidfile = self.out / "harness.pid"
        self.pidfile.write_text(str(os.getpid()))
        self.children = []
        self.records = []
        self.allowed = {}
        self.counts = {"pass": 0, "limited": 0, "fail": 0}

    def allow_limited(self, name, reason):
        """Permit one LIMITED case because the environment was observed to lack a capability."""
        self.allowed[name] = reason

    def exit_code(self, strict):
        """Non-zero when a case failed; `strict` also fails on unlisted LIMITED cases."""
        print(
            f"SUMMARY pass={self.counts['pass']} limited={self.counts['limited']} "
            f"fail={self.counts['fail']}",
            flush=True,
        )
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
            print(
                "STRICT   limited without a documented environment limit: "
                + ", ".join(unexpected),
                flush=True,
            )
            return 1
        return 0

    def record(self, test, status, detail):
        payload = {"test": test, "status": status, "detail": detail}
        self.records.append(payload)
        with self.results.open("a") as handle:
            handle.write(json.dumps(payload, default=str) + "\n")
        self.counts[status] = self.counts.get(status, 0) + 1
        print(f"{status.upper():8} {test}: {json.dumps(detail, default=str)[:300]}", flush=True)

    def spawn(self, command, env, log_name):
        log = (self.out / log_name).open("wb")
        process = subprocess.Popen(
            command, stdout=log, stderr=subprocess.STDOUT, env=env
        )
        self.children.append(process)
        return process

    def sizer(self, env, *args, timeout=40):
        result = subprocess.run(
            [self.args.sizer, *args], capture_output=True, text=True, env=env, timeout=timeout
        )
        parsed = None
        if "--json" in args and result.stdout.strip():
            try:
                parsed = json.loads(result.stdout)
            except json.JSONDecodeError:
                parsed = None
        return result, parsed


def session_env(display, root):
    runtime = root / "runtime"
    runtime.mkdir(parents=True, exist_ok=True)
    runtime.chmod(0o700)
    config = root / "config"
    data = root / "data"
    home = root / "home"
    for directory in (config, data, home):
        directory.mkdir(parents=True, exist_ok=True)
    # GTK asks the portal for the Inhibit interface on startup. With no backend
    # on the private bus that activation takes ~23s to fail, so the client's
    # first map is delayed by the same amount (see the enumerate deadline).
    return dict(
        os.environ,
        DISPLAY=display,
        GDK_BACKEND="x11",
        XDG_RUNTIME_DIR=str(runtime),
        XDG_CONFIG_HOME=str(config),
        XDG_DATA_HOME=str(data),
        XDG_DATA_DIRS="/usr/local/share:/usr/share",
        HOME=str(home),
        GSETTINGS_BACKEND="keyfile",
        NO_AT_BRIDGE="1",
        # No AT-SPI registry on the private bus; without this GTK would also
        # stall on the user's /run/user/1000/at-spi socket.
        GTK_A11Y="none",
    )


def delta(result, axis):
    return result["achieved"][axis] - result["requested"][axis]


def find_menu_window(env):
    """The popup sets WM_NAME=Sizer; the client window is titled SIZER-CLIENT."""
    result = subprocess.run(
        ["xdotool", "search", "--name", "^Sizer$"], capture_output=True, text=True, env=env
    )
    windows = [line for line in result.stdout.split() if line.isdigit()]
    return int(windows[0]) if windows else None


def find_menu_windows(env):
    """Every Sizer window: one per open menu level, parents before submenus."""
    result = subprocess.run(
        ["xdotool", "search", "--name", "^Sizer$"], capture_output=True, text=True, env=env
    )
    return [int(line) for line in result.stdout.split() if line.isdigit()]


def wait_for_menu(env, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        window = find_menu_window(env)
        if window:
            return window
        time.sleep(0.1)
    return None


def window_geometry(env, window):
    result = subprocess.run(
        ["xdotool", "getwindowgeometry", "--shell", str(window)],
        capture_output=True, text=True, env=env,
    )
    geometry = {}
    for line in result.stdout.splitlines():
        key, separator, value = line.partition("=")
        if not separator:
            continue
        try:
            geometry[key.strip()] = int(value)
        except ValueError:
            pass
    return geometry


def _xlib():
    """python-xlib is only needed for synthetic pointer/key input and the stub WM.

    `xdotool mousemove` is a no-op on this Xvfb (XTestFakeMotionEvent is
    accepted but the pointer does not move), while the raw XTEST calls from
    python-xlib do work, so input is injected through Xlib directly.
    """
    try:
        from Xlib import X, XK, Xatom, display as xdisplay
        from Xlib.ext import xtest
    except ImportError as error:  # pragma: no cover
        raise RuntimeError(
            "python-xlib is required for the menu tests (apt install python3-xlib)"
        ) from error
    return X, XK, Xatom, xdisplay, xtest


def move_pointer(display, x, y):
    X, _, _, xdisplay, xtest = _xlib()
    connection = xdisplay.Display(display)
    try:
        xtest.fake_input(connection, X.MotionNotify, x=x, y=y)
        connection.sync()
    finally:
        connection.close()


def click_pointer(display, button=1):
    X, _, _, xdisplay, xtest = _xlib()
    connection = xdisplay.Display(display)
    try:
        xtest.fake_input(connection, X.ButtonPress, button)
        xtest.fake_input(connection, X.ButtonRelease, button)
        connection.sync()
    finally:
        connection.close()


def press_keys(display, names):
    X, XK, _, xdisplay, xtest = _xlib()
    connection = xdisplay.Display(display)
    try:
        keycodes = [
            connection.keysym_to_keycode(XK.string_to_keysym(name)) for name in names
        ]
        for keycode in keycodes:
            xtest.fake_input(connection, X.KeyPress, keycode)
        for keycode in reversed(keycodes):
            xtest.fake_input(connection, X.KeyRelease, keycode)
        connection.sync()
    finally:
        connection.close()


def wait_for_input_grab(display, timeout=120):
    """Wait until the pointer can be grabbed on this display.

    mutter takes pointer and keyboard grabs itself while the session is active.
    Measured in this harness: the grab is still held 25s into a mutter session,
    and the menu's 1s retry (40 x 25ms) reports ALREADY_GRABBED. Whether mutter
    ever releases it was not established, so a caller must accept a menu that
    stays closed and report that outcome.
    """
    X, _, _, xdisplay, _ = _xlib()
    deadline = time.time() + timeout
    while time.time() < deadline:
        connection = xdisplay.Display(display)
        try:
            status = connection.screen().root.grab_pointer(
                False,
                X.ButtonPressMask | X.ButtonReleaseMask | X.PointerMotionMask,
                X.GrabModeAsync,
                X.GrabModeAsync,
                0,
                0,
                X.CurrentTime,
            )
            if status == X.GrabSuccess:
                connection.ungrab_pointer(X.CurrentTime)
                connection.sync()
                return True
        finally:
            connection.close()
        time.sleep(0.5)
    return False


class StubClient:
    """A window plus the two EWMH bits a window manager would provide.

    Synthetic pointer input never reaches a grab owner under mutter, so the
    click-to-apply chain is exercised on the bare server, where XTEST does work.
    This stub publishes _NET_CLIENT_LIST/_NET_ACTIVE_WINDOW and honours
    _NET_MOVERESIZE_WINDOW, so everything except the WM itself is the real code
    path.
    """

    def __init__(self, display, x=100, y=100, width=800, height=600):
        X, _, Xatom, xdisplay, _ = _xlib()
        self.connection = xdisplay.Display(display)
        screen = self.connection.screen()
        self.root = screen.root
        self.window = self.root.create_window(
            x, y, width, height, 0, screen.root_depth,
            X.InputOutput, X.CopyFromParent,
            background_pixel=screen.white_pixel,
            event_mask=X.StructureNotifyMask,
        )
        self.window.set_wm_name("SIZER-STUB")
        self.window.set_wm_class("sizer-stub", "SizerStub")
        self.window.map()
        self.moveresize = self.connection.intern_atom("_NET_MOVERESIZE_WINDOW")
        client_list = self.connection.intern_atom("_NET_CLIENT_LIST")
        active_window = self.connection.intern_atom("_NET_ACTIVE_WINDOW")
        self.root.change_attributes(
            event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask
        )
        self.root.change_property(client_list, Xatom.ATOM, 32, [self.window.id])
        self.root.change_property(active_window, Xatom.WINDOW, 32, [self.window.id])
        self.connection.sync()

    def pump(self, seconds):
        """Apply move/resize requests for a while, like a real WM would."""
        X = _xlib()[0]
        deadline = time.time() + seconds
        while time.time() < deadline:
            while self.connection.pending_events():
                event = self.connection.next_event()
                if event.type != X.ClientMessage or event.client_type != self.moveresize:
                    continue
                # sizer always sets x/y/w/h in the flags, so all four are used.
                _, x, y, width, height = event.data[1]
                self.window.configure(x=x, y=y, width=width, height=height)
                self.connection.sync()
            time.sleep(0.05)

    def size(self):
        geometry = self.window.get_geometry()
        return geometry.width, geometry.height

    def close(self):
        self.connection.close()


def ensure_private_bus():
    """gnome-shell refuses to start when the user's shell owns org.gnome.Shell on
    the session bus, so the harness re-executes itself on a private bus."""
    if os.environ.get("SIZER_E2E_PRIVATE_BUS") == "1":
        return
    env = dict(os.environ, SIZER_E2E_PRIVATE_BUS="1")
    os.execvpe(
        "dbus-run-session",
        ["dbus-run-session", "--", sys.executable, *sys.argv],
        env,
    )


def wait_shell_started(log_path, deadline):
    """A client mapped while mutter is still starting can hang unmapped forever
    (its MapRequest lands before the WM selects for it), so wait for the shell's
    own startup marker before creating test windows."""
    while time.time() < deadline:
        try:
            if "GNOME Shell started" in log_path.read_text(errors="replace"):
                return True
        except FileNotFoundError:
            pass
        time.sleep(0.2)
    return False


def wait_wm_ready(env, deadline):
    """mutter publishes _NET_WORKAREA before it is done claiming the WM role, so
    the client must not be started until EWMH moversize support is visible."""
    while time.time() < deadline:
        result = subprocess.run(
            ["xprop", "-root", "_NET_SUPPORTED"], capture_output=True, text=True, env=env
        )
        if "_NET_MOVERESIZE_WINDOW" in result.stdout and "_NET_CLIENT_LIST" in result.stdout:
            return True
        time.sleep(0.3)
    return False


def main():
    ensure_private_bus()
    parser = argparse.ArgumentParser()
    parser.add_argument("--sizer", required=True)
    parser.add_argument("--client", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--monitor", default="1600x900")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="treat LIMITED cases as failures (used by release gates)",
    )
    parser.add_argument(
        "--only",
        action="append",
        metavar="NAME",
        help="run only this standalone case and exit; repeatable. Names not in "
             "ONLY_CASES are refused instead of being silently reported as passing",
    )
    args = parser.parse_args()

    harness = Harness(args)
    display = free_display()
    session = harness.out / "session"
    env = session_env(display, session)
    xvfb = harness.spawn(
        ["Xvfb", display, "-screen", "0", f"{args.monitor}x24", "-nolisten", "tcp"], env, "xvfb.log"
    )
    shell = None
    client = None
    hotkey = None
    control = None
    control_harness = None
    macro_clients = []
    try:
        if not wait_display(display, time.time() + 10):
            harness.record("startup", "fail", {"display": display, "reason": "Xvfb not ready"})
            return 1
        shell = harness.spawn(
            ["gnome-shell", "--x11", "--mode", "user"], env, "shell.log"
        )

        deadline = time.time() + 25
        doctor = None
        while time.time() < deadline:
            result, parsed = harness.sizer(env, "doctor", "--json")
            if parsed and "monitors" in parsed:
                doctor = parsed
                break
            time.sleep(0.5)
        if not doctor:
            harness.record(
                "startup",
                "fail",
                {"display": display, "stderr": result.stderr[-600:], "stdout": result.stdout[-600:]},
            )
            return 1
        harness.record(
            "doctor",
            "pass" if doctor["capabilities"]["backend"] == "x11" else "fail",
            {
                "backend": doctor["capabilities"]["backend"],
                "behavior": doctor["behavior"],
                "monitors": doctor["monitors"],
                "work_area": doctor["active_work_area"],
            },
        )
        work = doctor["active_work_area"]

        if not wait_wm_ready(env, time.time() + 20):
            harness.record("wm_ready", "fail", {"reason": "_NET_MOVERESIZE_WINDOW not advertised"})
            return 1
        if not wait_shell_started(harness.out / "shell.log", time.time() + 25):
            harness.record("wm_ready", "fail", {"reason": "shell never logged 'GNOME Shell started'"})
            return 1
        time.sleep(1.0)

        client_command = [
            sys.executable,
            args.client,
            "--title",
            TITLE,
            "--width",
            "800",
            "--height",
            "600",
            # Minimum size hints make the oversized/undersized requests meaningful.
            "--min-width",
            "500",
            "--min-height",
            "400",
        ]
        client = harness.spawn(client_command, env, "client.log")
        # Portal activation on a backend-less private bus takes ~23s to fail, and
        # GTK waits for it before mapping, so allow well over that.
        deadline = time.time() + 45
        window = None
        while time.time() < deadline and not window:
            result, parsed = harness.sizer(env, "list", "--json")
            for entry in parsed or []:
                if entry.get("title") == TITLE:
                    window = entry
            if not window:
                time.sleep(0.4)
        if not window:
            clients = subprocess.run(
                ["xprop", "-root", "_NET_CLIENT_LIST"], capture_output=True, text=True, env=env
            )
            harness.record(
                "enumerate",
                "fail",
                {
                    "stdout": result.stdout[-600:],
                    "stderr": result.stderr[-400:],
                    "client_alive": client.poll() is None,
                    "client_log": (harness.out / "client.log").read_text()[-600:],
                    "xprop_client_list": clients.stdout.strip(),
                },
            )
            return 1
        wid = window["id"]
        harness.record(
            "enumerate",
            "pass" if window["geometry"]["client"]["w"] == 800 else "fail",
            {
                "id": wid,
                "app_id": window.get("app_id"),
                "frame": window["geometry"]["frame"],
                "client": window["geometry"]["client"],
                "decorated": window["geometry"]["decorated"],
            },
        )

        def case_macro_multi_window():
            nonlocal macro_clients
            # 7b. macros: a multi-window block really lays out every match, `$_index`
            #     and `$_count` follow the creation order, and the report is read back
            #     from the window manager instead of echoing the request.
            for suffix in ("A", "B"):
                macro_clients.append(
                    harness.spawn(
                        [
                            sys.executable,
                            args.client,
                            "--title",
                            f"MACRO-{suffix}",
                            "--width",
                            "600",
                            "--height",
                            "500",
                            "--min-width",
                            "500",
                            "--min-height",
                            "400",
                        ],
                        env,
                        f"macro-{suffix.lower()}.log",
                    )
                )
            deadline = time.time() + 60
            macro_titles = set()
            while time.time() < deadline:
                result, parsed = harness.sizer(env, "list", "--json")
                macro_titles = {
                    entry.get("title")
                    for entry in parsed or []
                    if str(entry.get("title", "")).startswith("MACRO-")
                }
                if macro_titles >= {"MACRO-A", "MACRO-B"}:
                    break
                time.sleep(0.4)
            macro = (
                "resize : proc[python3]:cap[MACRO-]:desktop*\n"
                "t = 120\n"
                "l = 100 + 540 * ($_index - 1)\n"
                "w = $_count * 250\n"
                "h = 400"
            )
            result, parsed = harness.sizer(env, "macro", macro, "--json")
            # Creation order is the host's order, not the spawn order: GTK maps the
            # two clients in whichever order the portal grants activation, so the
            # expectation is derived from the index each window was assigned.
            expectations = {
                outcome["title"]: {
                    "x": 100 + 540 * (outcome["index"] - 1),
                    "y": 120,
                    "w": 500,
                    "h": 400,
                }
                for outcome in (parsed or {}).get("windows", [])
            }
            # Read the geometry back through a separate listing: the command's own
            # report must agree with what the window manager holds now.
            result_after, listed = harness.sizer(env, "list", "--json")
            live = {
                entry["title"]: entry["geometry"]["client"]
                for entry in listed or []
                if str(entry.get("title", "")).startswith("MACRO-")
            }
            ok = (
                bool(parsed)
                and parsed.get("ok") is True
                and parsed.get("backend") == "x11"
                and len(parsed.get("windows", [])) == 2
                and sorted(
                    outcome["index"] for outcome in parsed.get("windows", [])
                ) == [1, 2]
                and all(
                    outcome.get("count") == 2
                    and outcome.get("exact") is True
                    and outcome.get("requested") == expectations[outcome["title"]]
                    and outcome.get("client") == expectations[outcome["title"]]
                    for outcome in parsed.get("windows", [])
                )
                and live == expectations
            )
            harness.record(
                "macro_multi_window",
                "pass" if ok else "fail",
                {
                    "titles": sorted(macro_titles),
                    "report": parsed,
                    "live": live,
                    "expected": expectations,
                    "stderr": result.stderr[-400:],
                    "list_stderr": result_after.stderr[-200:],
                },
            )

            # 7b2. `ignore_runtime_errors` decides whether a match failure aborts.
            result, parsed = harness.sizer(
                env,
                "macro",
                "option ignore_runtime_errors on\nresize : cap[NO-SUCH-WINDOW]* w = 100",
                "--json",
            )
            skipped_ok = (
                result.returncode == 0
                and bool(parsed)
                and parsed.get("windows") == []
                and len(parsed.get("skipped") or []) == 1
            )
            result_strict, _ = harness.sizer(
                env, "macro", "resize : cap[NO-SUCH-WINDOW]* w = 100"
            )
            strict_ok = (
                result_strict.returncode != 0
                and "no window matched" in result_strict.stderr
            )
            harness.record(
                "macro_runtime_errors_switch",
                "pass" if skipped_ok and strict_ok else "fail",
                {
                    "skipped": skipped_ok,
                    "strict": strict_ok,
                    "strict_returncode": result_strict.returncode,
                    "strict_stderr": result_strict.stderr[-300:],
                },
            )

        def case_config_apply():
            nonlocal macro_clients
            # 13. config-driven preset apply: label lookup, anchor and basis from TOML
            config_dir = session / "config" / "sizer"
            config_dir.mkdir(parents=True, exist_ok=True)
            (config_dir / "config.toml").write_text(CONFIG_TEXT)
            check_result, check_parsed = harness.sizer(env, "config", "check", "--json")
            apply_result, apply_parsed = harness.sizer(
                env, "apply", "center-700", "--window", f"title:{TITLE}", "--json"
            )
            client_rect = apply_parsed["result"]["achieved_client"] if apply_parsed else {}
            used_area = apply_parsed.get("area", {}) if apply_parsed else {}
            centered = bool(client_rect) and (
                abs(client_rect["x"] + client_rect["w"] // 2 - (used_area["x"] + used_area["w"] // 2)) <= 1
                and abs(client_rect["y"] + client_rect["h"] // 2 - (used_area["y"] + used_area["h"] // 2)) <= 1
            )
            applied = (
                check_result.returncode == 0
                and client_rect.get("w") == 700
                and client_rect.get("h") == 500
                and centered
            )
            harness.record(
                "config_apply",
                "pass" if applied else "fail",
                {
                    "check": check_parsed,
                    "client": client_rect,
                    "work_area": work,
                    "stderr": (check_result.stderr + apply_result.stderr)[-300:],
                },
            )

        def case_preset_copy_and_separator():
            nonlocal macro_clients
            # 13b. preset copy and menu separators round-trip through the file and the
            #      copy is still a real, applicable preset.
            # Self-contained: write the base configuration instead of reusing
            # whatever case 13 left behind.
            config_dir = session / "config" / "sizer"
            config_dir.mkdir(parents=True, exist_ok=True)
            (config_dir / "config.toml").write_text(CONFIG_TEXT)
            copy_result, _ = harness.sizer(env, "preset", "copy", "center-700", "--json")
            separator_result, _ = harness.sizer(
                env, "preset", "separator", "--group", "16:9", "--json"
            )
            check_after, _ = harness.sizer(env, "config", "check", "--json")
            _, listed = harness.sizer(env, "preset", "list", "--json")
            entries = (listed or {}).get("presets", [])
            by_label = {entry["label"]: entry for entry in entries if entry.get("label")}
            separators = [entry for entry in entries if entry.get("kind") == "separator"]
            separator_index = next(
                (index for index, entry in enumerate(entries) if entry.get("kind") == "separator"),
                None,
            )
            # A separator has no label, so `#IDX` is how the CLI addresses it. `set`
            # on one must fail instead of writing a preset with an empty label.
            set_separator = remove_result = check_removed = None
            separators_after = None
            if separator_index is not None:
                set_separator, _ = harness.sizer(
                    env, "preset", "set", f"#{separator_index + 1}", "--size", "10x10", "--json"
                )
                remove_result, _ = harness.sizer(
                    env, "preset", "remove", f"#{separator_index + 1}", "--json"
                )
                check_removed, _ = harness.sizer(env, "config", "check", "--json")
                _, listed_after = harness.sizer(env, "preset", "list", "--json")
                separators_after = [
                    entry
                    for entry in (listed_after or {}).get("presets", [])
                    if entry.get("kind") == "separator"
                ]
            copy_apply, copy_applied = harness.sizer(
                env, "apply", "center-700 copy", "--window", f"title:{TITLE}", "--json"
            )
            copy_rect = (copy_applied or {}).get("result", {}).get("achieved_client", {})
            copied = (
                copy_result.returncode == 0
                and separator_result.returncode == 0
                and check_after.returncode == 0
                and by_label.get("center-700 copy", {}).get("size") == "700x500"
                and by_label.get("center-700 copy", {}).get("hotkey") is None
                and len(separators) == 1
                and separators[0].get("group") == "16:9"
                and separator_index is not None
                and set_separator.returncode != 0
                and "separator" in set_separator.stderr
                and remove_result.returncode == 0
                and check_removed.returncode == 0
                and separators_after == []
                and copy_apply.returncode == 0
                and copy_rect.get("w") == 700
                and copy_rect.get("h") == 500
            )
            harness.record(
                "preset_copy_and_separator",
                "pass" if copied else "fail",
                {
                    "copy": copy_result.stderr[-200:],
                    "separator": separator_result.stderr[-200:],
                    "check": check_after.returncode,
                    "labels": sorted(by_label),
                    "separators": separators,
                    "set_separator_error": set_separator.stderr[-200:] if set_separator else None,
                    "removed_row": separator_index + 1 if separator_index is not None else None,
                    "check_after_remove": check_removed.returncode if check_removed else None,
                    "separators_after_remove": separators_after,
                    "copy_apply": copy_rect,
                },
            )

        def case_preset_absolute_position():
            nonlocal macro_clients
            # 13c. an absolute position is a first-class preset field: it arrives
            #      from the TOML file, is editable with `preset set --x/--y`, and
            #      `resize`/`move` accept the same coordinates on the command line.
            config_dir = session / "config" / "sizer"
            config_dir.mkdir(parents=True, exist_ok=True)
            (config_dir / "config.toml").write_text(POSITION_CONFIG_TEXT)
            check_result, _ = harness.sizer(env, "config", "check", "--json")

            apply_result, applied = harness.sizer(
                env, "apply", "pinned", "--window", f"title:{TITLE}", "--json"
            )
            area = (applied or {}).get("area") or {}
            pinned = (applied or {}).get("result", {}).get("achieved_client", {})
            pinned_ok = (
                check_result.returncode == 0
                and apply_result.returncode == 0
                and pinned.get("x") == area.get("x", 0) + 120
                and pinned.get("y") == area.get("y", 0) + 70
                and pinned.get("w") == 500
                and pinned.get("h") == 400
            )

            set_result, _ = harness.sizer(
                env, "preset", "set", "center-700", "--x", "250", "--y", "40", "--json"
            )
            _, listed = harness.sizer(env, "preset", "list", "--json")
            entry = next(
                (
                    preset
                    for preset in (listed or {}).get("presets", [])
                    if preset.get("label") == "center-700"
                ),
                {},
            )
            moved_apply, moved = harness.sizer(
                env, "apply", "center-700", "--window", f"title:{TITLE}", "--json"
            )
            moved_rect = (moved or {}).get("result", {}).get("achieved_client", {})
            set_ok = (
                set_result.returncode == 0
                and entry.get("x") == 250
                and entry.get("y") == 40
                and entry.get("anchor") is None
                and moved_apply.returncode == 0
                and moved_rect.get("x") == area.get("x", 0) + 250
                and moved_rect.get("y") == area.get("y", 0) + 40
                and moved_rect.get("w") == 700
                and moved_rect.get("h") == 500
            )

            # Setting an anchor again replaces the position instead of leaving a
            # pair that `config check` would reject.
            anchor_result, _ = harness.sizer(
                env, "preset", "set", "center-700", "--anchor", "center", "--json"
            )
            _, restored = harness.sizer(env, "preset", "list", "--json")
            restored_entry = next(
                (
                    preset
                    for preset in (restored or {}).get("presets", [])
                    if preset.get("label") == "center-700"
                ),
                {},
            )
            center_apply, centered = harness.sizer(
                env, "apply", "center-700", "--window", f"title:{TITLE}", "--json"
            )
            center_rect = (centered or {}).get("result", {}).get("achieved_client", {})
            anchor_ok = (
                anchor_result.returncode == 0
                and restored_entry.get("anchor") == "center"
                and restored_entry.get("x") is None
                and restored_entry.get("y") is None
                and center_apply.returncode == 0
                and center_rect.get("x") == area.get("x", 0) + (area.get("w", 0) - 700) // 2
                and center_rect.get("y") == area.get("y", 0) + (area.get("h", 0) - 500) // 2
            )

            resize_result, resized = harness.sizer(
                env,
                "resize",
                "--window",
                f"title:{TITLE}",
                "--size",
                "620x460",
                "--x",
                "40",
                "--y",
                "120",
                "--json",
            )
            resize_rect = (resized or {}).get("result", {}).get("achieved_client", {})
            move_result, moved_cli = harness.sizer(
                env,
                "move",
                "--window",
                f"title:{TITLE}",
                "--x",
                "60",
                "--y",
                "140",
                "--json",
            )
            move_rect = (moved_cli or {}).get("result", {}).get("achieved_client", {})
            cli_ok = (
                resize_result.returncode == 0
                and resize_rect.get("x") == area.get("x", 0) + 40
                and resize_rect.get("y") == area.get("y", 0) + 120
                and resize_rect.get("w") == 620
                and resize_rect.get("h") == 460
                and move_result.returncode == 0
                and move_rect.get("x") == area.get("x", 0) + 60
                and move_rect.get("y") == area.get("y", 0) + 140
                and move_rect.get("w") == 620
                and move_rect.get("h") == 460
            )

            # A move-only preset (Sizer's <relocate>) keeps the current size.
            add_move, _ = harness.sizer(
                env, "preset", "add", "--label", "only-move", "--size", "",
                "--x", "300", "--y", "260", "--json"
            )
            move_only_apply, move_only = harness.sizer(
                env, "apply", "only-move", "--window", f"title:{TITLE}", "--json"
            )
            move_only_rect = (move_only or {}).get("result", {}).get("achieved_client", {})
            move_only_ok = (
                add_move.returncode == 0
                and move_only_apply.returncode == 0
                and move_only_rect.get("x") == area.get("x", 0) + 300
                and move_only_rect.get("y") == area.get("y", 0) + 260
                and move_only_rect.get("w") == 620
                and move_only_rect.get("h") == 460
            )

            # Half a position, or an anchor beside one, is refused up front.
            half_x, _ = harness.sizer(
                env, "preset", "set", "small-640", "--x", "10", "--json"
            )
            both, _ = harness.sizer(
                env,
                "resize",
                "--window",
                f"title:{TITLE}",
                "--size",
                "300x200",
                "--anchor",
                "center",
                "--x",
                "10",
                "--y",
                "20",
                "--json",
            )
            errors_ok = (
                half_x.returncode != 0
                and "needs --y" in half_x.stderr
                and both.returncode != 0
                and "mutually exclusive" in both.stderr
            )

            positioned = (
                pinned_ok and set_ok and anchor_ok and cli_ok and move_only_ok and errors_ok
            )
            harness.record(
                "preset_absolute_position",
                "pass" if positioned else "fail",
                {
                    "area": area,
                    "pinned": pinned,
                    "pinned_ok": pinned_ok,
                    "set": entry,
                    "set_ok": set_ok,
                    "restored": restored_entry,
                    "anchor_ok": anchor_ok,
                    "resized": resize_rect,
                    "moved": move_rect,
                    "cli_ok": cli_ok,
                    "move_only": move_only_rect,
                    "move_only_ok": move_only_ok,
                    "half_x_error": half_x.stderr.strip()[-160:],
                    "both_error": both.stderr.strip()[-160:],
                    "errors_ok": errors_ok,
                },
            )

        def case_preset_outside_bounds():
            nonlocal macro_clients
            # 13d. allowOutsideBounds: off pulls the request into the area here, so
            #      both platforms behave alike; on writes the coordinates verbatim
            #      and reports `outside`, leaving `exact` to describe the readback.
            config_dir = session / "config" / "sizer"
            config_dir.mkdir(parents=True, exist_ok=True)
            (config_dir / "config.toml").write_text(OUTSIDE_CONFIG_TEXT)
            check_result, _ = harness.sizer(env, "config", "check", "--json")

            clamp_apply, clamped = harness.sizer(
                env, "apply", "clamped", "--window", f"title:{TITLE}", "--json"
            )
            clamp_area = (clamped or {}).get("area") or {}
            clamp_result = (clamped or {}).get("result") or {}
            clamp_req = clamp_result.get("requested") or {}
            clamp_got = clamp_result.get("achieved_client") or {}
            clamp_ok = (
                check_result.returncode == 0
                and clamp_apply.returncode == 0
                and clamp_req.get("x") == clamp_area.get("x")
                and clamp_req.get("y") == clamp_area.get("y", 0) + 300
                and clamp_result.get("clamped") is True
                and clamp_result.get("outside") is False
                and clamp_result.get("exact") is True
                and clamp_got.get("x") == clamp_req.get("x")
                and clamp_got.get("y") == clamp_req.get("y")
            )

            allow_apply, allowed = harness.sizer(
                env, "apply", "allowed", "--window", f"title:{TITLE}", "--json"
            )
            allow_result = (allowed or {}).get("result") or {}
            allow_req = allow_result.get("requested") or {}
            allow_got = allow_result.get("achieved_client") or {}
            allow_ok = (
                allow_apply.returncode == 0
                and allow_req.get("x") == clamp_area.get("x", 0) - 120
                and allow_req.get("y") == clamp_area.get("y", 0) + 300
                and allow_result.get("outside") is True
                # `exact` must describe the readback, not the intent: mutter is
                # free to keep the window inside, and the result has to say so.
                and allow_result.get("exact") == (allow_got == allow_req)
            )

            # The command-line flags win over both the preset and the option.
            forced_apply, forced = harness.sizer(
                env, "apply", "clamped", "--allow-outside",
                "--window", f"title:{TITLE}", "--json"
            )
            forced_result = (forced or {}).get("result") or {}
            forced_req = forced_result.get("requested") or {}
            denied_apply, denied = harness.sizer(
                env, "apply", "allowed", "--no-allow-outside",
                "--window", f"title:{TITLE}", "--json"
            )
            denied_result = (denied or {}).get("result") or {}
            denied_req = denied_result.get("requested") or {}
            flags_ok = (
                forced_apply.returncode == 0
                and forced_req.get("x") == clamp_area.get("x", 0) - 120
                and forced_result.get("outside") is True
                and denied_apply.returncode == 0
                and denied_req.get("x") == clamp_area.get("x")
                and denied_result.get("outside") is False
                and denied_result.get("clamped") is True
                and denied_result.get("exact") is True
            )

            # The global option alone is enough when the preset says nothing.
            (config_dir / "config.toml").write_text(OUTSIDE_GLOBAL_CONFIG_TEXT)
            global_apply, global_applied = harness.sizer(
                env, "apply", "clamped", "--window", f"title:{TITLE}", "--json"
            )
            global_result = (global_applied or {}).get("result") or {}
            global_req = global_result.get("requested") or {}
            global_ok = (
                global_apply.returncode == 0
                and global_req.get("x") == clamp_area.get("x", 0) - 120
                and global_result.get("outside") is True
            )

            passed = clamp_ok and allow_ok and flags_ok and global_ok
            harness.record(
                "preset_outside_bounds",
                "pass" if passed else "fail",
                {
                    "area": clamp_area,
                    "clamped": {"ok": clamp_ok, "requested": clamp_req,
                                "achieved": clamp_got,
                                "clamped": clamp_result.get("clamped"),
                                "outside": clamp_result.get("outside")},
                    "allowed": {"ok": allow_ok, "requested": allow_req,
                                "achieved": allow_got,
                                "outside": allow_result.get("outside"),
                                "exact": allow_result.get("exact")},
                    "flags": {"ok": flags_ok, "forced": forced_req,
                              "denied": denied_req,
                              "denied_outside": denied_result.get("outside")},
                    "global": {"ok": global_ok, "requested": global_req,
                               "outside": global_result.get("outside")},
                },
            )

        # `--only NAME` runs the named cases from this fresh session and exits.
        # A name that is not registered is refused, never silently skipped.
        ONLY_CASES = {
            "macro_multi_window": case_macro_multi_window,
            "config_apply": case_config_apply,
            "preset_copy_and_separator": case_preset_copy_and_separator,
            "preset_absolute_position": case_preset_absolute_position,
            "preset_outside_bounds": case_preset_outside_bounds,
        }
        if args.only:
            unknown = [name for name in args.only if name not in ONLY_CASES]
            if unknown:
                harness.record("case_selection", "fail",
                               {"unknown": unknown, "supported": sorted(ONLY_CASES)})
                return harness.exit_code(args.strict)
            for name in args.only:
                ONLY_CASES[name]()
            return harness.exit_code(args.strict)
        # 1. exact client resize
        for name, size in (("900x700", "900x700"), ("777x555", "777x555")):
            result, parsed = harness.sizer(
                env, "resize", "--size", size, "--window", f"title:{TITLE}", "--json"
            )
            ok = parsed and delta(parsed["result"], "w") == 0 and delta(parsed["result"], "h") == 0
            harness.record(
                f"resize_client_{name}",
                "pass" if ok else "fail",
                {"result": parsed["result"] if parsed else None, "stderr": result.stderr[-300:]},
            )

        # 2. client size hints
        result, parsed = harness.sizer(
            env, "resize", "--size", "1x1", "--window", f"title:{TITLE}", "--json"
        )
        flags = parsed["result"] if parsed else {}
        ok = bool(parsed) and flags["adjusted_by_hints"] and flags["achieved"]["w"] >= 1
        harness.record("resize_min_hints", "pass" if ok else "fail", {"result": flags})

        # 3. oversized request is clamped, not silently accepted
        result, parsed = harness.sizer(
            env, "resize", "--size", "8000x6000", "--window", f"title:{TITLE}", "--json"
        )
        flags = parsed["result"] if parsed else {}
        ok = (
            bool(parsed)
            and flags["clamped"]
            and flags["achieved_frame"]["w"] <= work["w"]
            and flags["achieved_frame"]["h"] <= work["h"]
        )
        harness.record("resize_clamped", "pass" if ok else "fail", {"result": flags, "work_area": work})

        # 4. maximized window is restored before the resize
        result, parsed = harness.sizer(
            env, "state", "--window", f"title:{TITLE}", "--json"
        )
        maximized = bool(parsed) and parsed.get("maximized") is True
        result, parsed = harness.sizer(
            env, "resize", "--size", "700x500", "--window", f"title:{TITLE}", "--json"
        )
        flags = parsed["result"] if parsed else {}
        ok = (
            maximized
            and bool(parsed)
            and flags["unmaximized"]
            and flags["achieved"]["w"] == 700
            and flags["achieved"]["h"] == 500
        )
        harness.record(
            "resize_unmaximizes",
            "pass" if ok else "fail",
            {"was_maximized": maximized, "result": flags},
        )

        # 5. move with frame semantics: right/bottom edges touch the work area
        result, parsed = harness.sizer(
            env,
            "move",
            "--anchor",
            "br",
            "--basis",
            "frame",
            "--window",
            f"title:{TITLE}",
            "--json",
        )
        frame = parsed["result"]["achieved_frame"] if parsed else {}
        ok = (
            bool(parsed)
            and frame["x"] + frame["w"] == work["x"] + work["w"]
            and frame["y"] + frame["h"] == work["y"] + work["h"]
        )
        harness.record(
            "move_bottom_right_frame",
            "pass" if ok else "fail",
            {"frame": frame, "work_area": work, "stderr": result.stderr[-300:]},
        )

        # 6. above on/off
        result_on, parsed_on = harness.sizer(
            env, "above", "--window", f"title:{TITLE}", "--json"
        )
        above_on = bool(parsed_on) and parsed_on.get("above") is True
        result_off, parsed_off = harness.sizer(
            env, "above", "--off", "--window", f"title:{TITLE}", "--json"
        )
        above_off = bool(parsed_off) and parsed_off.get("above") is False
        state_prop = subprocess.run(
            ["xprop", "-id", str(wid), "_NET_WM_STATE"],
            capture_output=True, text=True, env=env,
        ).stdout.strip()
        harness.record(
            "above_toggle", "pass" if above_on and above_off else "fail",
            {
                "on": above_on,
                "off": above_off,
                "on_raw": result_on.stdout.strip()[-200:],
                "off_raw": result_off.stdout.strip()[-200:],
                "off_err": result_off.stderr.strip()[-300:],
                "net_wm_state": state_prop[:300],
            },
        )

        # 7. decoration control is reported as unavailable: mutter ignores
        #    _MOTIF_WM_HINTS once a window is mapped.
        harness.record(
            "decoration_capability",
            "pass" if doctor["capabilities"]["decorations"] is False else "fail",
            {"capabilities": doctor["capabilities"]},
        )

        case_macro_multi_window()

        # 7c. state actions from a macro match the window's own state.
        def list_state(title):
            """Read state without applying anything: `sizer state` sets it."""
            _, entries = harness.sizer(env, "list", "--json")
            return next(
                (entry for entry in entries or [] if entry.get("title") == title), {}
            )

        maximize_result, maximized_report = harness.sizer(
            env, "macro", "resize : cap[MACRO-A] maximize", "--json"
        )
        maximized = list_state("MACRO-A").get("maximized") is True
        restore_result, restored_report = harness.sizer(
            env, "macro", "resize : cap[MACRO-A] restore", "--json"
        )
        restored = list_state("MACRO-A").get("maximized") is False
        harness.record(
            "macro_state_actions",
            "pass" if maximized and restored else "fail",
            {
                "maximized": maximized,
                "restored": restored,
                "maximize": {
                    "returncode": maximize_result.returncode,
                    "stderr": maximize_result.stderr[-300:],
                    "report": maximized_report,
                },
                "restore": {
                    "returncode": restore_result.returncode,
                    "stderr": restore_result.stderr[-300:],
                    "report": restored_report,
                },
            },
        )

        # 7d. a macro outside the published productions fails loudly, with the
        #     source position, instead of being silently ignored.
        result, _ = harness.sizer(env, "macro", "resize : proc[no-such-window] t = 1 bogus")
        rejected = (
            result.returncode != 0
            and "Unexpected token in macro" in result.stderr
            and "line 1" in result.stderr
        )
        harness.record(
            "macro_invalid_token_is_red",
            "pass" if rejected else "fail",
            {"returncode": result.returncode, "stderr": result.stderr[-300:]},
        )

        # 8. hotkey conflict is diagnosed, not silent
        hotkey = subprocess.Popen(
            [args.sizer, "hotkey", "--accelerator", "super+alt+F10", "--hold", "6"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        harness.children.append(hotkey)
        time.sleep(1.2)
        result, parsed = harness.sizer(
            env, "hotkey", "--accelerator", "super+alt+F10", "--json"
        )
        outcome = parsed["outcome"] if parsed else {}
        conflict = isinstance(outcome, dict) and outcome.get("state") == "conflict"
        harness.record(
            "hotkey_conflict",
            "pass" if conflict else "fail",
            {"second": parsed, "stderr": result.stderr[-300:]},
        )
        stop(hotkey, timeout=10)

        # 9. delivery probe on a bare X server (no window manager preempting keys)
        control_display = free_display()
        control_env = session_env(control_display, harness.out / "session-control")
        # -noreset keeps the pointer where the menu tests put it: this server has
        # no long-lived client of its own, and a reset would re-centre the pointer.
        control = harness.spawn(
            [
                "Xvfb", control_display, "-screen", "0", "1024x768x24",
                "-nolisten", "tcp", "-noreset",
            ],
            control_env, "xvfb-control.log",
        )
        if wait_display(control_display, time.time() + 10):
            time.sleep(0.8)
            result, parsed = harness.sizer(
                control_env, "hotkey", "--accelerator", "super+alt+F10",
                "--verify-delivery", "--json",
            )
            outcome = parsed["outcome"] if parsed else {}
            verified = isinstance(outcome, dict) and outcome.get("state") == "verified"
            harness.record(
                "hotkey_delivery_control_no_wm",
                "pass" if verified else "fail",
                {"outcome": outcome, "stderr": result.stderr[-300:]},
            )
        else:
            harness.record("hotkey_delivery_control_no_wm", "fail", {"display": control_display})

        # 11. two logical monitors through RandR 1.5, then select by index
        split = subprocess.run(
            ["xrandr", "--setmonitor", "sizer-left", "800/212x900/238+0+0", "none"],
            env=env, capture_output=True, text=True,
        )
        split2 = subprocess.run(
            ["xrandr", "--setmonitor", "sizer-right", "800/212x900/238+800+0", "none"],
            env=env, capture_output=True, text=True,
        )
        time.sleep(1.5)
        # The automatic full-screen monitor would otherwise take index 0.
        subprocess.run(["xrandr", "--delmonitor", "screen"], env=env, capture_output=True)
        time.sleep(1.0)
        result, parsed = harness.sizer(env, "doctor", "--json")
        monitors = parsed["monitors"] if parsed else []
        names = [monitor["name"] for monitor in monitors]
        ok_split = (
            split.returncode == 0
            and split2.returncode == 0
            and "sizer-left" in names
            and "sizer-right" in names
        )
        placement = None
        if ok_split:
            right_index = names.index("sizer-right")
            result, parsed = harness.sizer(
                env, "resize", "--size", "600x500", "--anchor", "tl",
                "--monitor", str(right_index), "--window", f"title:{TITLE}", "--json",
            )
            placement = parsed["result"]["achieved_frame"] if parsed else None
        on_right = bool(placement) and placement["x"] >= 800
        if not ok_split:
            # The server itself refused the split: record why, and let `--strict`
            # accept this one case because the harness observed the gap here.
            reason = "xrandr refused to define two logical monitors: " + \
                (split.stderr + split2.stderr).strip()[:200]
            for name in ("multi_monitor_select", "monitor_active_follows_pointer"):
                harness.allow_limited(name, reason)
        harness.record(
            "multi_monitor_select",
            "pass" if ok_split and on_right else ("limited" if not ok_split else "fail"),
            {
                "monitors": names,
                "second_monitor_frame": placement,
                "xrandr": (split.stderr + split2.stderr).strip()[:300],
                "reason": None if ok_split else
                "xrandr refused to define two logical monitors on this server",
            },
        )
        # 11b. `monitor=active` follows the pointer and prefers the specific
        #      half over the screen-wide automatic monitor that mutter keeps.
        #      The window is the same in both probes, so only the pointer differs.
        active_probes = {}
        if ok_split:
            for name, (px, py), expected_x in (("left", (200, 400), 0), ("right", (1200, 400), 800)):
                move_pointer(display, px, py)
                time.sleep(0.3)
                _, parsed = harness.sizer(
                    env, "resize", "--size", "600x500", "--anchor", "tl",
                    "--monitor", "active", "--window", f"title:{TITLE}", "--json",
                )
                frame = parsed["result"]["achieved_frame"] if parsed else {}
                active_probes[name] = {"pointer": [px, py], "frame": frame,
                                       "ok": frame.get("x") == expected_x}
        harness.record(
            "monitor_active_follows_pointer",
            "pass" if ok_split and all(probe["ok"] for probe in active_probes.values())
            else ("limited" if not ok_split else "fail"),
            {"probes": active_probes,
             "reason": None if ok_split else
             "needs two logical monitors, which this server cannot define"},
        )
        subprocess.run(["xrandr", "--delmonitor", "sizer-right"], env=env, capture_output=True)
        subprocess.run(["xrandr", "--delmonitor", "sizer-left"], env=env, capture_output=True)

        # 12. delivery probe under the WM: Phase 0 says this can legitimately fail
        result, parsed = harness.sizer(
            env, "hotkey", "--accelerator", "super+alt+F11", "--verify-delivery", "--json",
            timeout=60,
        )
        outcome = parsed["outcome"] if parsed else {}
        harness.record(
            "hotkey_delivery_with_wm",
            "pass"
            if isinstance(outcome, dict) and outcome.get("state") == "verified"
            else "limited",
            {"outcome": outcome},
        )
        case_config_apply()

        case_preset_copy_and_separator()

        case_preset_absolute_position()

        case_preset_outside_bounds()

        # 14. every configured hotkey registers, and a taken one is reported as
        #     a conflict instead of being silently swallowed.
        # Self-contained: the hotkey configuration is written here, not assumed
        # from the cases above.
        config_dir = session / "config" / "sizer"
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.toml").write_text(CONFIG_TEXT)
        result, parsed = harness.sizer(env, "--json", "hotkeys")
        states = [
            entry.get("report", {}).get("outcome", {}).get("state") for entry in parsed or []
        ]
        # Two preset hotkeys plus options.hotkey, which opens the cursor menu.
        registered = states == ["registered_unverified"] * 3
        holder = subprocess.Popen(
            [args.sizer, "hotkey", "--accelerator", "super+alt+F8", "--hold", "5"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        harness.children.append(holder)
        time.sleep(1.2)
        conflict_result, conflict_parsed = harness.sizer(env, "--json", "hotkeys")
        conflicted = conflict_result.returncode != 0 and any(
            entry.get("report", {}).get("outcome", {}).get("state") == "conflict"
            for entry in conflict_parsed or []
        )
        harness.record(
            "config_hotkeys",
            "pass" if registered and conflicted else "fail",
            {
                "registered_states": states,
                "returncode": result.returncode,
                "conflict_returncode": conflict_result.returncode,
                "conflict_stderr": conflict_result.stderr[-300:],
            },
        )
        stop(holder, timeout=10)

        # 15. the cursor menu opens at the pointer and a row click applies that
        #     preset. Pointer injection never reaches a grab owner under mutter,
        #     so this runs on the bare server, where XTEST does work; StubClient
        #     supplies the EWMH bits a window manager would.
        control_config = harness.out / "session-control" / "config" / "sizer"
        control_config.mkdir(parents=True, exist_ok=True)
        (control_config / "config.toml").write_text(CONFIG_TEXT)
        stub = StubClient(control_display, 200, 150)
        try:
            stub.pump(0.2)
            move_pointer(control_display, 400, 300)
            popup = subprocess.Popen(
                [args.sizer, "popup", "--json"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=control_env,
            )
            harness.children.append(popup)
            menu_window = wait_for_menu(control_env, 6)
            menu = window_geometry(control_env, menu_window) if menu_window else {}
            clicked = False
            if menu:
                time.sleep(0.2)
                # Row geometry: one drawn border plus 8px padding above the rows.
                # The third row is the Chinese label, so this also covers
                # measuring and drawing a label that is not Latin-1.
                row_height = (menu["HEIGHT"] - 2 * (1 + 8)) // 3
                row_y = menu["Y"] + 1 + 8 + 2 * row_height + row_height // 2
                move_pointer(control_display, menu["X"] + 30, row_y)
                click_pointer(control_display)
                clicked = True
                stub.pump(3)
            try:
                popup_out, popup_err = popup.communicate(timeout=25)
            except subprocess.TimeoutExpired:
                stop(popup)
                popup_out, popup_err = popup.communicate()
                popup_err = f"menu did not close\n{popup_err}"
            selection = None
            if popup_out.strip().startswith("{"):
                selection = json.loads(popup_out)
            at_pointer = bool(menu) and menu.get("X") == 400 and menu.get("Y") == 300
            chosen = selection["result"]["achieved_client"] if selection else {}
            ok = (
                at_pointer
                and clicked
                and popup.returncode == 0
                and chosen.get("w") == 500
                and chosen.get("h") == 300
            )
            harness.record(
                "popup_click_applies",
                "pass" if ok else "fail",
                {
                    "menu_geometry": menu,
                    "selection": selection,
                    "returncode": popup.returncode,
                    "stderr": popup_err[-300:],
                },
            )
        finally:
            stub.close()

        # 16. Escape on the bare server dismisses the menu without a selection
        #     and without touching any window.
        move_pointer(control_display, 500, 350)
        popup = subprocess.Popen(
            [args.sizer, "popup", "--json"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=control_env,
        )
        harness.children.append(popup)
        menu_window = wait_for_menu(control_env, 6)
        menu = window_geometry(control_env, menu_window) if menu_window else {}
        if menu_window:
            time.sleep(0.2)
            press_keys(control_display, ["Escape"])
        try:
            popup_out, popup_err = popup.communicate(timeout=25)
        except subprocess.TimeoutExpired:
            stop(popup)
            popup_out, popup_err = popup.communicate()
            popup_err = f"menu did not close\n{popup_err}"
        dismissed = json.loads(popup_out) if popup_out.strip().startswith("{") else None
        harness.record(
            "popup_escape_dismisses",
            "pass"
            if menu.get("X") == 500
            and menu.get("Y") == 350
            and dismissed == {"selected": None}
            and popup.returncode == 0
            else "fail",
            {
                "menu_geometry": menu,
                "dismissed": dismissed,
                "returncode": popup.returncode,
                "stderr": popup_err[-300:],
            },
        )

        # 16b. `sizer menu`, the desktop entry's command, falls back to the
        #      cursor menu when no shell extension is on the bus. The bus is
        #      removed from the environment so the result does not depend on
        #      whatever the person running the suite has on their session bus.
        menu_env = dict(control_env)
        menu_env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        move_pointer(control_display, 520, 360)
        menu_cmd = subprocess.Popen(
            [args.sizer, "menu", "--json"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=menu_env,
        )
        harness.children.append(menu_cmd)
        menu_window = wait_for_menu(menu_env, 6)
        menu = window_geometry(menu_env, menu_window) if menu_window else {}
        if menu_window:
            time.sleep(0.2)
            press_keys(control_display, ["Escape"])
        try:
            menu_out, menu_err = menu_cmd.communicate(timeout=25)
        except subprocess.TimeoutExpired:
            stop(menu_cmd)
            menu_out, menu_err = menu_cmd.communicate()
            menu_err = f"menu did not close\n{menu_err}"
        dismissed = json.loads(menu_out) if menu_out.strip().startswith("{") else None
        harness.record(
            "menu_falls_back_to_popup",
            "pass"
            if menu.get("X") == 520
            and menu.get("Y") == 360
            and dismissed == {"selected": None}
            and menu_cmd.returncode == 0
            else "fail",
            {
                "menu_geometry": menu,
                "dismissed": dismissed,
                "returncode": menu_cmd.returncode,
                "stderr": menu_err[-300:],
            },
        )

        # 17. the same menu while mutter owns the input devices. Two contracts,
        #     picked by whether the pointer can be grabbed at all: a grabbable
        #     pointer must open the menu, and an owned pointer must be refused
        #     explicitly instead of the menu appearing unarmed.
        grab_ready = wait_for_input_grab(display, 20)
        active_before = subprocess.run(
            ["xprop", "-root", "_NET_ACTIVE_WINDOW"], capture_output=True, text=True, env=env
        ).stdout.strip()
        move_pointer(display, 600, 400)
        popup = subprocess.Popen(
            [args.sizer, "popup", "--json"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        harness.children.append(popup)
        menu_window = wait_for_menu(env, 6)
        menu = window_geometry(env, menu_window) if menu_window else {}
        if menu_window:
            time.sleep(0.2)
            press_keys(display, ["Escape"])
        try:
            popup_out, popup_err = popup.communicate(timeout=25)
        except subprocess.TimeoutExpired:
            stop(popup)
            popup_out, popup_err = popup.communicate()
            popup_err = f"menu did not close\n{popup_err}"
        active_after = subprocess.run(
            ["xprop", "-root", "_NET_ACTIVE_WINDOW"], capture_output=True, text=True, env=env
        ).stdout.strip()
        dismissed = json.loads(popup_out) if popup_out.strip().startswith("{") else None
        opened = (
            menu.get("X") == 600
            and menu.get("Y") == 400
            and dismissed == {"selected": None}
            and active_before == active_after
            and popup.returncode == 0
        )
        refused = (
            not menu_window
            and popup.returncode != 0
            and "holds the pointer" in popup_err
            and active_before == active_after
        )
        ok = opened if grab_ready else refused
        harness.record(
            "popup_grabbable_pointer",
            "pass" if ok else "fail",
            {
                "grab_ready": grab_ready,
                "menu_geometry": menu,
                "dismissed": dismissed,
                "active_unchanged": active_before == active_after,
                "returncode": popup.returncode,
                "expectation": "menu opens at the pointer" if grab_ready
                else "the ungrabbable pointer is refused with a protocol error",
                "stderr": popup_err[-200:],
            },
        )

        # 18. options.hotkey opens the menu. A bare server is used because
        #     synthetic keys reach passive grabs there (test 9); under the WM the
        #     same injection is a Phase 0 limitation.
        menu_holder = subprocess.Popen(
            [args.sizer, "hotkeys", "--hold", "8"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=control_env,
        )
        harness.children.append(menu_holder)
        time.sleep(1.5)
        move_pointer(control_display, 300, 200)
        press_keys(control_display, ["Super_L", "Alt_L", "z"])
        menu_window = wait_for_menu(control_env, 6)
        if menu_window:
            time.sleep(0.2)
            press_keys(control_display, ["Escape"])
        stop(menu_holder, timeout=10)
        harness.record(
            "menu_hotkey_opens_popup",
            "pass" if menu_window else "fail",
            {"menu_window": menu_window},
        )

        # 19. with no configuration file at all, popup falls back to the built-in
        #     preset list, so the primary interaction works on a fresh install.
        noconfig = harness.out / "session-noconfig" / "config"
        noconfig.mkdir(parents=True, exist_ok=True)
        noconfig_env = dict(control_env, XDG_CONFIG_HOME=str(noconfig))
        stub = StubClient(control_display, 240, 180)
        try:
            stub.pump(0.2)
            move_pointer(control_display, 360, 260)
            popup = subprocess.Popen(
                [args.sizer, "popup", "--json"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=noconfig_env,
            )
            harness.children.append(popup)
            menu_window = wait_for_menu(noconfig_env, 6)
            menu = window_geometry(noconfig_env, menu_window) if menu_window else {}
            clicked = False
            if menu:
                time.sleep(0.2)
                # Built-in rows: Center, 50% x 50%, then the 4:3 / 16:9 / 16:10
                # groups. Row 1 halves the 1024x768 control screen.
                row_height = (menu["HEIGHT"] - 2 * (1 + 8)) // 5
                row_y = menu["Y"] + 1 + 8 + row_height + row_height // 2
                move_pointer(control_display, menu["X"] + 30, row_y)
                click_pointer(control_display)
                clicked = True
                stub.pump(3)
            try:
                popup_out, popup_err = popup.communicate(timeout=25)
            except subprocess.TimeoutExpired:
                stop(popup)
                popup_out, popup_err = popup.communicate()
                popup_err = f"menu did not close\n{popup_err}"
            selection = json.loads(popup_out) if popup_out.strip().startswith("{") else None
            chosen = selection["result"]["achieved_client"] if selection else {}
            ok = (
                clicked
                and popup.returncode == 0
                and chosen.get("w") == 512
                and chosen.get("h") == 384
                and "using built-in presets" in popup_err
            )
            harness.record(
                "popup_builtin_presets",
                "pass" if ok else "fail",
                {
                    "menu_geometry": menu,
                    "selection": selection,
                    "returncode": popup.returncode,
                    "stderr": popup_err[-300:],
                },
            )
        finally:
            stub.close()

        # 20. the built-in menu hotkey is Ctrl+Win+Z, with no configuration file.
        menu_holder = subprocess.Popen(
            [args.sizer, "hotkeys", "--hold", "8"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=noconfig_env,
        )
        harness.children.append(menu_holder)
        time.sleep(1.5)
        move_pointer(control_display, 300, 240)
        press_keys(control_display, ["Super_L", "Control_L", "z"])
        menu_window = wait_for_menu(noconfig_env, 6)
        if menu_window:
            time.sleep(0.2)
            press_keys(control_display, ["Escape"])
        stop(menu_holder, timeout=10)
        harness.record(
            "builtin_hotkey_opens_menu",
            "pass" if menu_window else "fail",
            {"menu_window": menu_window, "accelerator": "<Primary><Super>z"},
        )
        # 21. grouped presets become a cascading submenu in the cursor menu, and
        #     the menu is re-read from the configuration file on every launch.
        grouped_home = harness.out / "session-grouped" / "config"
        grouped_conf = grouped_home / "sizer"
        grouped_conf.mkdir(parents=True, exist_ok=True)
        (grouped_conf / "config.toml").write_text(GROUPED_CONFIG_TEXT)
        grouped_env = dict(control_env, XDG_CONFIG_HOME=str(grouped_home))

        def row_center(menu, index, rows):
            row_height = (menu["HEIGHT"] - 2 * (1 + 8)) // rows
            return menu["X"] + 30, menu["Y"] + 1 + 8 + index * row_height + row_height // 2

        def wait_submenu(env_, parent, parent_geo):
            """The submenu opens flush against the parent's right border."""
            deadline = time.time() + 4
            while time.time() < deadline:
                for window in find_menu_windows(env_):
                    if window == parent:
                        continue
                    geometry = window_geometry(env_, window)
                    if geometry.get("X") == parent_geo["X"] + parent_geo["WIDTH"] - 1:
                        return window, geometry
                time.sleep(0.1)
            return None, {}

        def popup_result(popup):
            try:
                out, err = popup.communicate(timeout=25)
            except subprocess.TimeoutExpired:
                stop(popup)
                out, err = popup.communicate()
                err = f"menu did not close\n{err}"
            payload = json.loads(out) if out.strip().startswith("{") else None
            return payload, err

        stub = StubClient(control_display, 260, 200)
        try:
            stub.pump(0.2)
            move_pointer(control_display, 420, 300)
            popup = subprocess.Popen(
                [args.sizer, "popup", "--json"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=grouped_env,
            )
            harness.children.append(popup)
            root = wait_for_menu(grouped_env, 6)
            root_geo = window_geometry(grouped_env, root) if root else {}
            submenu_geo = {}
            submenu = None
            if root_geo:
                time.sleep(0.2)
                # Row 0 is the "16:9 >" group; clicking it opens the submenu.
                move_pointer(control_display, *row_center(root_geo, 0, 2))
                click_pointer(control_display)
                submenu, submenu_geo = wait_submenu(grouped_env, root, root_geo)
                if submenu:
                    # Row 1 of the submenu is "wide-480".
                    move_pointer(control_display, *row_center(submenu_geo, 1, 2))
                    click_pointer(control_display)
                    stub.pump(3)
            selection, popup_err = popup_result(popup)
            chosen = selection["result"]["achieved_client"] if selection else {}
            aligned = bool(submenu_geo) and submenu_geo.get("Y") == root_geo.get("Y")
            grouped_ok = (
                aligned
                and popup.returncode == 0
                and chosen.get("w") == 854
                and chosen.get("h") == 480
            )
            harness.record(
                "popup_group_submenu",
                "pass" if grouped_ok else "fail",
                {
                    "root_geometry": root_geo,
                    "submenu_geometry": submenu_geo,
                    "row_aligned": aligned,
                    "selection": selection,
                    "returncode": popup.returncode,
                    "stderr": popup_err[-300:],
                },
            )

            # Rewriting the file is enough: the next launch shows the new list.
            (grouped_conf / "config.toml").write_text(SOLO_CONFIG_TEXT)
            move_pointer(control_display, 380, 280)
            popup = subprocess.Popen(
                [args.sizer, "popup", "--json"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=grouped_env,
            )
            harness.children.append(popup)
            root = wait_for_menu(grouped_env, 6)
            solo_geo = window_geometry(grouped_env, root) if root else {}
            if solo_geo:
                time.sleep(0.2)
                move_pointer(control_display, *row_center(solo_geo, 0, 1))
                click_pointer(control_display)
                stub.pump(3)
            selection, popup_err = popup_result(popup)
            chosen = selection["result"]["achieved_client"] if selection else {}
            changed_ok = (
                popup.returncode == 0
                and chosen.get("w") == 620
                and chosen.get("h") == 460
            )
            harness.record(
                "popup_follows_config_edit",
                "pass" if changed_ok else "fail",
                {
                    "geometry": solo_geo,
                    "selection": selection,
                    "returncode": popup.returncode,
                    "stderr": popup_err[-300:],
                },
            )

            # A separator occupies a row: the preset below it is row 2 of 3 and
            # clicking it proves the line did not swallow the click.
            (grouped_conf / "config.toml").write_text(SEPARATOR_CONFIG_TEXT)
            move_pointer(control_display, 380, 280)
            popup = subprocess.Popen(
                [args.sizer, "popup", "--json"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=grouped_env,
            )
            harness.children.append(popup)
            root = wait_for_menu(grouped_env, 6)
            separator_geo = window_geometry(grouped_env, root) if root else {}
            if separator_geo:
                time.sleep(0.2)
                move_pointer(control_display, *row_center(separator_geo, 2, 3))
                click_pointer(control_display)
                stub.pump(3)
            selection, popup_err = popup_result(popup)
            chosen = selection["result"]["achieved_client"] if selection else {}
            separator_ok = (
                popup.returncode == 0
                and chosen.get("w") == 620
                and chosen.get("h") == 460
            )
            harness.record(
                "popup_menu_separator",
                "pass" if separator_ok else "fail",
                {
                    "geometry": separator_geo,
                    "selection": selection,
                    "returncode": popup.returncode,
                    "stderr": popup_err[-300:],
                },
            )
        finally:
            stub.close()

        return harness.exit_code(args.strict)
    finally:
        for process in [*macro_clients, control, hotkey, client, shell, xvfb]:
            stop(process)
        pidfile = harness.out / "harness.pid"
        if pidfile.exists():
            pidfile.unlink()


if __name__ == "__main__":
    sys.exit(main())
