// GNOME Shell / Mutter API differences between supported releases.
//
// Everything version dependent lives here so the extension itself stays free of
// scattered feature checks. Measured on GNOME 46.0 and 50.1 (Phase 0/1):
//   - Meta.Window.get_maximized() was removed in 50.
//   - Meta.Window.is_client_decorated() was removed in 50.
//   - Meta.is_wayland_compositor() was removed in 50 together with the X11 backend.
//   - Meta.Window.decorated is read-only on both (GNOME windows use CSD).

import Meta from 'gi://Meta';
import * as Config from 'resource:///org/gnome/shell/misc/config.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

export const SHELL_VERSION = String(Config.PACKAGE_VERSION ?? 'unknown');
export const SHELL_MAJOR = Number.parseInt(SHELL_VERSION.split('.')[0], 10) || 0;

const MAXIMIZE_HORIZONTAL = 1;
const MAXIMIZE_VERTICAL = 2;

/// Run `fn`, returning `fallback` when the binding or state is gone.
export function safe(fn, fallback = null) {
    try {
        return fn();
    } catch (e) {
        return fallback;
    }
}

export function isWayland() {
    if (typeof Meta.is_wayland_compositor === 'function')
        return safe(() => Meta.is_wayland_compositor(), true);
    // Mutter 50 dropped the X11 backend, so the symbol is gone and the session
    // can only be Wayland. Guessing `false` here would hide the real session type.
    return true;
}

/// Maximize flags as a number, or null when the release exposes neither form.
export function maximized(win) {
    if (typeof win.get_maximized === 'function')
        return safe(() => win.get_maximized(), null);
    if ('maximized' in win)
        return safe(() => win.maximized, null);
    const horizontal = safe(() => win.maximized_horizontally, null);
    const vertical = safe(() => win.maximized_vertically, null);
    if (horizontal !== null || vertical !== null)
        return (horizontal ? MAXIMIZE_HORIZONTAL : 0) | (vertical ? MAXIMIZE_VERTICAL : 0);
    return null;
}

/// Whether the window draws its own decorations, or null when unknown.
export function clientDecorated(win) {
    if (typeof win.is_client_decorated === 'function')
        return safe(() => win.is_client_decorated(), null);
    // Removed in GNOME 50. `decorated` describes server-side decorations, so it
    // cannot answer this question; unknown is reported instead of a guess.
    return null;
}

/// Shell container that hosts shell UI; the menu and its anchor actor go here.
export function uiGroup() {
    return Main.uiGroup ?? Main.layoutManager.uiGroup;
}

/// Monitor index containing a point, falling back to the current monitor.
///
/// Compared against each monitor geometry instead of building a rectangle:
/// `Meta.Rectangle` is gone in GNOME 50 (geometry types moved to Mtk).
export function monitorAt(display, x, y) {
    const count = safe(() => display.get_n_monitors(), 0);
    for (let index = 0; index < count; index++) {
        const geometry = safe(() => display.get_monitor_geometry(index), null);
        if (geometry &&
            x >= geometry.x && x < geometry.x + geometry.width &&
            y >= geometry.y && y < geometry.y + geometry.height)
            return index;
    }
    return safe(() => display.get_current_monitor(), 0);
}

/// Work area of a monitor, falling back to its full geometry.
export function workArea(display, monitor) {
    return safe(() => Main.layoutManager.getWorkAreaForMonitor(monitor), null) ??
        safe(() => display.get_monitor_geometry(monitor), null);
}

/// Frame rect that puts `client` where asked.
///
/// `move_resize_frame` takes a frame rect while presets are expressed on the
/// client rect. `client_rect_to_frame_rect` looks like the direct answer, but it
/// returns the rect unchanged while the window is maximized or still settling
/// (measured on GNOME 46: identity during the transition, the 28x29 margin only
/// once the state settled), which writes the wrong frame. The margin from
/// `frame_rect_to_client_rect` is stable in both states, so the offset is taken
/// from the window's current frame instead.
export function clientToFrameRect(win, client) {
    const frame = safe(() => win.get_frame_rect(), null);
    const current = frame ? safe(() => win.frame_rect_to_client_rect(frame), null) : null;
    if (frame && current) {
        return {
            x: client.x + (frame.x - current.x),
            y: client.y + (frame.y - current.y),
            width: client.width + (frame.width - current.width),
            height: client.height + (frame.height - current.height),
        };
    }
    return safe(() => win.client_rect_to_frame_rect(client), client);
}

/// Whether the window is already gone.
///
/// `Meta.Window` has no `is_destroyed()` on the releases we support (only
/// `Meta.WindowActor` does), and touching a destroyed window raises a GLib
/// critical, which aborts a session running with `G_DEBUG=fatal-criticals`. The
/// display's window list is the authoritative liveness source and cannot throw.
export function isDestroyed(win) {
    if (!win)
        return true;
    return safe(() => !global.display.list_all_windows().includes(win), true);
}

/// What this shell build can actually do, for `sizer doctor`.
export function capabilities() {
    return {
        shell_version: SHELL_VERSION,
        shell_major: SHELL_MAJOR,
        wayland: isWayland(),
        maximized_getter: safe(() => typeof Meta.Window.prototype.get_maximized === 'function', false),
        client_decorated_getter: safe(
            () => typeof Meta.Window.prototype.is_client_decorated === 'function', false),
        // GNOME windows are always client-side decorated and `decorated` is read only.
        decoration_control: false,
        geometry_write: 'move_resize_frame',
        stay_above: true,
        menu_in_shell: true,
    };
}
