// Sizer's GNOME Shell layer.
//
// The extension is deliberately thin: it enumerates windows, applies geometry
// and state, registers the menu keybinding, hosts the popup menu and exposes a
// D-Bus API. Presets, macros and configuration live in the Rust daemon.
//
// Release-to-release API differences live in compat.js, not here.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import {
    capabilities, clientDecorated, clientToFrameRect, isDestroyed, maximized, monitorAt,
    safe, uiGroup, workArea,
} from './compat.js';
import {PresetQueue} from './preset_state.js';

const BUS_NAME = 'org.sizerlinux.Shell';
const OBJECT_PATH = '/org/sizerlinux/Shell';
const KEYBINDING_NAME = 'toggle-hotkey';

// Classic Sizer presets, used until the daemon pushes the configuration, in the
// same grouped form `Config::builtin` uses.
const DEFAULT_PRESETS = [
    {label: 'Center', size: '', anchor: 'center'},
    {label: '50% x 50%', size: '50%x50%', anchor: 'center'},
    {label: '800x600', size: '800x600', anchor: 'center', group: '4:3'},
    {label: '1024x768', size: '1024x768', anchor: 'center', group: '4:3'},
    {label: '1280x960', size: '1280x960', anchor: 'center', group: '4:3'},
    {label: '1600x1200', size: '1600x1200', anchor: 'center', group: '4:3'},
    {label: '1280x720', size: '1280x720', anchor: 'center', group: '16:9'},
    {label: '1600x900', size: '1600x900', anchor: 'center', group: '16:9'},
    {label: '1920x1080', size: '1920x1080', anchor: 'center', group: '16:9'},
    {label: '2560x1440', size: '2560x1440', anchor: 'center', group: '16:9'},
    {label: '1280x800', size: '1280x800', anchor: 'center', group: '16:10'},
    {label: '1440x900', size: '1440x900', anchor: 'center', group: '16:10'},
    {label: '1680x1050', size: '1680x1050', anchor: 'center', group: '16:10'},
    {label: '1920x1200', size: '1920x1200', anchor: 'center', group: '16:10'},
];

// Anchor names as Rust writes them (`bottom_right`) plus the one-letter form.
const ANCHORS = {
    topleft: 'tl', tl: 'tl',
    top: 't', t: 't',
    topright: 'tr', tr: 'tr',
    left: 'l', l: 'l',
    center: 'c', centre: 'c', c: 'c',
    right: 'r', r: 'r',
    bottomleft: 'bl', bl: 'bl',
    bottom: 'b', b: 'b',
    bottomright: 'br', br: 'br',
};

// Keycodes of the evdev/us layout, only used to report whether the configured
// accelerator is bound (KeybindingState).
const KEYCODES = {
    q: 24, w: 25, e: 26, r: 27, t: 28, y: 29, u: 30, i: 31, o: 32, p: 33,
    a: 38, s: 39, d: 40, f: 41, g: 42, h: 43, j: 44, k: 45, l: 46,
    z: 52, x: 53, c: 54, v: 55, b: 56, n: 57, m: 58,
    '1': 10, '2': 11, '3': 12, '4': 13, '5': 14, '6': 15, '7': 16, '8': 17, '9': 18, '0': 19,
};

/// Parse `1920x1080` or `50%x50%`.
function normalizeSize(text) {
    const value = String(text ?? '').trim().toLowerCase();
    const parts = value.split('x');
    if (parts.length !== 2)
        return null;
    const [rawWidth, rawHeight] = parts.map(part => part.trim());
    if (rawWidth.endsWith('%') && rawHeight.endsWith('%')) {
        const width = Number.parseFloat(rawWidth.slice(0, -1));
        const height = Number.parseFloat(rawHeight.slice(0, -1));
        if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0)
            return null;
        return {percent: true, width, height};
    }
    if (!/^\d+$/.test(rawWidth) || !/^\d+$/.test(rawHeight))
        return null;
    const width = Number.parseInt(rawWidth, 10);
    const height = Number.parseInt(rawHeight, 10);
    if (width <= 0 || height <= 0)
        return null;
    return {percent: false, width, height};
}

/// Split a group path into trimmed, non-empty segments.
function groupPath(raw) {
    return String(raw ?? '')
        .split('/')
        .map(part => part.trim())
        .filter(Boolean);
}

/// Normalize a monitor selector: null for the active monitor, `primary`, or an
/// index. Returns `undefined` for a value the extension cannot honour.
function normalizeMonitor(raw) {
    if (raw === undefined || raw === null)
        return null;
    const text = String(raw).trim().toLowerCase();
    if (!text || text === 'active')
        return null;
    if (text === 'primary')
        return 'primary';
    if (/^\d+$/.test(text))
        return Number.parseInt(text, 10);
    return undefined;
}

/// Validate one `{label, size, anchor, group, monitor}` entry, or return null.
function normalizePreset(raw) {
    if (!raw || typeof raw !== 'object')
        return null;
    if (raw.kind === 'separator')
        return {kind: 'separator', label: '', size: null, anchor: null, position: null,
                group: groupPath(raw.group), monitor: null};
    const label = String(raw.label ?? '').trim();
    const position = normalizePosition(raw.x, raw.y);
    const size = normalizeSize(raw.size);
    const anchorName = String(raw.anchor ?? '').trim().toLowerCase().replace(/[-_ ]/g, '');
    const anchor = anchorName ? ANCHORS[anchorName] : null;
    if (anchorName && !anchor)
        return null;
    // No size is the move-only form (Sizer's <relocate>): the window keeps its
    // current size, and a position or an anchor says where to move it.
    if (!label || (!size && !position && !anchor))
        return null;
    if (position === undefined || (position && anchor))
        return null;
    const monitor = normalizeMonitor(raw.monitor);
    if (monitor === undefined)
        return null;
    const allowOutsideBounds = typeof raw.allow_outside === 'boolean' ? raw.allow_outside : null;
    if (allowOutsideBounds !== null && !position)
        return null;
    return {kind: 'resize', label, size, anchor, position, allowOutsideBounds,
            group: groupPath(raw.group), monitor};
}

/// `[x, y]` when both are integers, `null` when neither is given, and
/// `undefined` for half a position (which would place the window by accident).
function normalizePosition(x, y) {
    const hasX = x !== null && x !== undefined;
    const hasY = y !== null && y !== undefined;
    if (!hasX && !hasY)
        return null;
    if (!hasX || !hasY)
        return undefined;
    if (!Number.isInteger(x) || !Number.isInteger(y))
        return undefined;
    return [x, y];
}

/// Why `normalizePreset` rejects an entry, for the daemon's report.
function presetProblem(raw) {
    if (!raw || typeof raw !== 'object')
        return 'not an object';
    if (raw.kind === 'separator')
        return 'separator';
    if (!String(raw.label ?? '').trim())
        return 'label must not be empty';
    const anchorName = String(raw.anchor ?? '').trim().toLowerCase().replace(/[-_ ]/g, '');
    // No size keeps the current window size, so the preset still needs a
    // position or an anchor to know where to move the window.
    if (!normalizeSize(raw.size) && normalizePosition(raw.x, raw.y) === null && !anchorName)
        return 'size must look like 1024x768 or 50%x50% unless x and y or an anchor are given';
    if (anchorName && !ANCHORS[anchorName])
        return `unknown anchor ${JSON.stringify(raw.anchor)}`;
    const position = normalizePosition(raw.x, raw.y);
    if (position === undefined)
        return 'x and y must be integers set together';
    if (position && anchorName)
        return 'anchor and x/y are mutually exclusive';
    if (raw.allow_outside !== undefined && raw.allow_outside !== null
            && typeof raw.allow_outside !== 'boolean')
        return 'allow_outside must be a boolean';
    if (raw.allow_outside === true && !position)
        return 'allow_outside needs x and y';
    if (normalizeMonitor(raw.monitor) === undefined)
        return `unknown monitor ${JSON.stringify(raw.monitor)}`;
    return 'invalid preset';
}

/// Preset menu tree.
///
/// Mirrors `sizer_config::menu`: groups keep first-appearance order, paths merge
/// by trimmed name, one submenu level per path segment, a separator keeps its
/// position, and a group with a single preset stays a group.
function menuTree(presets) {
    const nodes = [];
    presets.forEach((preset, index) => insertNode(
        nodes, preset.group, index, preset.label, preset.kind));
    return nodes;
}

function insertNode(nodes, path, index, label, kind) {
    if (!path.length) {
        nodes.push(kind === 'separator'
            ? {kind: 'separator'}
            : {kind: 'preset', index, label});
        return;
    }
    let group = nodes.find(node => node.kind === 'group' && node.label === path[0]);
    if (!group) {
        group = {kind: 'group', label: path[0], children: []};
        nodes.push(group);
    }
    insertNode(group.children, path.slice(1), index, label, kind);
}

/// Preset labels in depth-first menu order, the order `ActivateMenuRow` uses.
function presetLabels(nodes) {
    const labels = [];
    const walk = (items) => {
        for (const node of items) {
            if (node.kind === 'preset')
                labels.push(node.label);
            else if (node.kind === 'group')
                walk(node.children);
        }
    };
    walk(nodes);
    return labels;
}

function countGroups(nodes) {
    let groups = 0;
    for (const node of nodes) {
        if (node.kind === 'group') {
            groups += 1 + countGroups(node.children);
        }
    }
    return groups;
}

/// Client size for a preset, resolving percentages against the work area.
/// Pull a window into the area without changing its size; sizes stay the
/// compositor's business. Mirrors `Rect::clamp_into` in sizer-core.
function clampOrigin(origin, size, area) {
    const maxX = area.x + area.width - size.w;
    const maxY = area.y + area.height - size.h;
    return {
        x: size.w >= area.width ? area.x : Math.min(Math.max(origin.x, area.x), maxX),
        y: size.h >= area.height ? area.y : Math.min(Math.max(origin.y, area.y), maxY),
    };
}

function withinArea(rect, area) {
    return rect.x >= area.x && rect.y >= area.y
        && rect.x + rect.w <= area.x + area.width
        && rect.y + rect.h <= area.y + area.height;
}

function resolveSize(size, area) {
    if (!size.percent)
        return {w: size.width, h: size.height};
    return {
        w: Math.max(1, Math.round(area.width * size.width / 100)),
        h: Math.max(1, Math.round(area.height * size.height / 100)),
    };
}

/// Top-left corner for a client rect of `w`x`h` at `anchor`.
///
/// Mirrors `sizer_core::Anchor::place`: integer division, so both sides agree.
function anchorPosition(area, w, h, anchor) {
    const right = area.x + area.width;
    const bottom = area.y + area.height;
    const centerX = area.x + Math.floor(area.width / 2);
    const centerY = area.y + Math.floor(area.height / 2);
    switch (anchor) {
    case 'tl':
        return {x: area.x, y: area.y};
    case 't':
        return {x: centerX - Math.floor(w / 2), y: area.y};
    case 'tr':
        return {x: right - w, y: area.y};
    case 'l':
        return {x: area.x, y: centerY - Math.floor(h / 2)};
    case 'c':
        return {x: centerX - Math.floor(w / 2), y: centerY - Math.floor(h / 2)};
    case 'r':
        return {x: right - w, y: centerY - Math.floor(h / 2)};
    case 'bl':
        return {x: area.x, y: bottom - h};
    case 'b':
        return {x: centerX - Math.floor(w / 2), y: bottom - h};
    case 'br':
        return {x: right - w, y: bottom - h};
    default:
        return null;
    }
}

/// Keycode and modifier mask of an accelerator such as `<Primary><Super>z`.
function parseAccelerator(text) {
    const source = String(text ?? '');
    const parts = (source.includes('<')
        ? source.replace(/>/g, '<').split('<')
        : source.split('+'))
        .map(part => part.trim())
        .filter(Boolean);
    if (parts.length < 2)
        return null;
    const key = parts.pop().toLowerCase();
    let mask = 0;
    for (const part of parts) {
        const name = part.replace(/[<>]/g, '').toLowerCase();
        if (name === 'primary' || name === 'control' || name === 'ctrl')
            mask |= 4;
        else if (name === 'super' || name === 'win' || name === 'mod4')
            mask |= 64;
        else if (name === 'alt' || name === 'mod1')
            mask |= 8;
        else if (name === 'shift')
            mask |= 1;
        else
            return null;
    }
    let keycode = KEYCODES[key];
    const functionKey = /^f(\d{1,2})$/.exec(key);
    if (keycode === undefined && functionKey) {
        const number = Number.parseInt(functionKey[1], 10);
        if (number >= 1 && number <= 10)
            keycode = 66 + number;
        else if (number === 11)
            keycode = 95;
        else if (number === 12)
            keycode = 96;
    }
    if (keycode === undefined)
        return null;
    return {keycode, mask};
}

const IFACE_XML = `
<node>
  <interface name="org.sizerlinux.Shell">
    <method name="Ping">
      <arg type="s" direction="out" name="reply"/>
    </method>
    <method name="ShellInfo">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="ListWindows">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="GetWindow">
      <arg type="s" direction="in" name="id"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="MoveResizeFrame">
      <arg type="s" direction="in" name="id"/>
      <arg type="i" direction="in" name="x"/>
      <arg type="i" direction="in" name="y"/>
      <arg type="i" direction="in" name="w"/>
      <arg type="i" direction="in" name="h"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="MoveResize">
      <arg type="s" direction="in" name="id"/>
      <arg type="i" direction="in" name="x"/>
      <arg type="i" direction="in" name="y"/>
      <arg type="i" direction="in" name="w"/>
      <arg type="i" direction="in" name="h"/>
      <arg type="s" direction="in" name="basis"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="MoveFrame">
      <arg type="s" direction="in" name="id"/>
      <arg type="i" direction="in" name="x"/>
      <arg type="i" direction="in" name="y"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="SetAbove">
      <arg type="s" direction="in" name="id"/>
      <arg type="b" direction="in" name="above"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="SetMaximized">
      <arg type="s" direction="in" name="id"/>
      <arg type="b" direction="in" name="maximized"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="SetMinimized">
      <arg type="s" direction="in" name="id"/>
      <arg type="b" direction="in" name="minimized"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="Activate">
      <arg type="s" direction="in" name="id"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="LastWindow">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="SetDecorated">
      <arg type="s" direction="in" name="id"/>
      <arg type="b" direction="in" name="decorated"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="KeybindingState">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="ShowMenu">
      <arg type="i" direction="in" name="x"/>
      <arg type="i" direction="in" name="y"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="CloseMenu">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="SetPresets">
      <arg type="s" direction="in" name="json"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="MenuState">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="ActivateMenuRow">
      <arg type="i" direction="in" name="index"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="LastResult">
      <arg type="s" direction="out" name="json"/>
    </method>
    <signal name="Changed">
      <arg type="s" name="json"/>
    </signal>
  </interface>
</node>`;

function rectToObj(rect) {
    if (!rect)
        return null;
    return {x: rect.x, y: rect.y, w: rect.width, h: rect.height};
}

/// Whether two `{x, y, w, h}` rects are the same, used to tell a settled window
/// from one the compositor is still moving.
function sameRect(a, b) {
    return !!a && !!b && a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;
}

export default class SizerExtension extends Extension {
    enable() {
        this._keyCount = 0;
        this._lastKeyTime = null;
        this._menuSection = null;
        this._menuAnchor = null;
        this._menuItems = [];
        this._menuGeometry = null;
        this._presets = DEFAULT_PRESETS.map(normalizePreset);
        this._menuTree = menuTree(this._presets);
        this._presetLabels = presetLabels(this._menuTree);
        // Global `[options]` the daemon pushes; the X11 defaults until then.
        this._options = {area: 'workarea', basis: 'client'};
        this._lastResult = null;
        // Tracks which preset request is still the newest one and which windows
        // are waiting on the compositor (see preset_state.js).
        this._presetQueue = new PresetQueue();
        this._lastApply = null;
        // One timer per applied request: a verification still owed to window A
        // must not be dropped because window B was resized a moment later.
        this._verifyTimers = new Set();
        this._geometryTimer = 0;
        this._dbusImpl = null;
        this._nameOwnerId = 0;
        this._settings = this.getSettings();
        this._connectedWindows = new Map();

        this._exportDbus();
        this._registerKeybinding();

        this._windowCreatedId = global.display.connect('window-created', () => {
            this._notifyWindows();
            this._emitChanged('window-created');
        });
        this._notifyWindows();
    }

    disable() {
        if (this._windowCreatedId) {
            global.display.disconnect(this._windowCreatedId);
            this._windowCreatedId = 0;
        }
        this._disconnectWindows();
        this._closeMenu();
        this._presetQueue.cancelAll();
        for (const id of this._verifyTimers)
            GLib.source_remove(id);
        this._verifyTimers.clear();

        try {
            Main.wm.removeKeybinding(KEYBINDING_NAME);
        } catch (e) {
            console.warn(`[sizer] removeKeybinding failed: ${e}`);
        }

        if (this._dbusImpl) {
            this._dbusImpl.unexport();
            this._dbusImpl = null;
        }
        if (this._nameOwnerId) {
            Gio.bus_unown_name(this._nameOwnerId);
            this._nameOwnerId = 0;
        }

        this._connectedWindows.clear();
        this._settings = null;
    }

    // ---------------------------------------------------------------- D-Bus

    _exportDbus() {
        this._dbusImpl = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbusImpl.export(Gio.DBus.session, OBJECT_PATH);
        this._nameOwnerId = Gio.bus_own_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
            null, null, null);
    }

    _emitChanged(reason, result = null) {
        if (!this._dbusImpl)
            return;
        const payload = {reason, windows: this._listWindows()};
        if (result)
            payload.result = result;
        this._dbusImpl.emit_signal('Changed', new GLib.Variant('(s)', [JSON.stringify(payload)]));
    }

    Ping() {
        return 'pong';
    }

    ShellInfo() {
        const caps = capabilities();
        const info = {
            bus_name: BUS_NAME,
            object_path: OBJECT_PATH,
            shell_version: caps.shell_version,
            capabilities: caps,
            monitors: [],
            current_monitor: global.display.get_current_monitor(),
            primary_monitor: global.display.get_primary_monitor(),
            n_workspaces: global.workspace_manager.get_n_workspaces(),
            current_workspace: safe(
                () => global.workspace_manager.get_active_workspace_index(), 0),
            // Number of windows whose geometry signals this extension holds;
            // lets a test prove that closed windows are dropped again.
            tracked_windows: this._connectedWindows.size,
        };
        for (let i = 0; i < global.display.get_n_monitors(); i++) {
            info.monitors.push({
                index: i,
                geometry: rectToObj(global.display.get_monitor_geometry(i)),
                work_area: rectToObj(workArea(global.display, i)),
                primary: i === info.primary_monitor,
                scale: global.display.get_monitor_scale(i),
            });
        }
        return JSON.stringify(info);
    }

    ListWindows() {
        return JSON.stringify(this._listWindows());
    }

    GetWindow(id) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        return JSON.stringify({ok: true, window: this._windowInfo(win)});
    }

    MoveResizeFrame(id, x, y, w, h) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        const before = this._windowInfo(win);
        let error = null;
        try {
            win.move_resize_frame(true, x, y, w, h);
        } catch (e) {
            error = String(e);
        }
        return JSON.stringify({
            ok: error === null,
            error,
            requested: {x, y, w, h},
            before,
            immediate: this._windowInfo(win),
        });
    }

    MoveFrame(id, x, y) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        const before = this._windowInfo(win);
        let error = null;
        try {
            win.move_frame(true, x, y);
        } catch (e) {
            error = String(e);
        }
        return JSON.stringify({
            ok: error === null,
            error,
            requested: {x, y},
            before,
            immediate: this._windowInfo(win),
        });
    }

    SetAbove(id, above) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        if (above)
            win.make_above();
        else
            win.unmake_above();
        return JSON.stringify({ok: true, above: win.is_above(), window: this._windowInfo(win)});
    }

    SetMaximized(id, maximizedWanted) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        if (maximizedWanted)
            win.maximize(Meta.MaximizeFlags.BOTH);
        else
            win.unmaximize(Meta.MaximizeFlags.BOTH);
        return JSON.stringify({
            ok: true,
            maximized: maximized(win),
            window: this._windowInfo(win),
        });
    }

    SetMinimized(id, minimizedWanted) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        try {
            if (minimizedWanted)
                win.minimize();
            else
                win.unminimize();
        } catch (e) {
            return JSON.stringify({ok: false, error: String(e), id});
        }
        return JSON.stringify({
            ok: true,
            minimized: safe(() => win.minimized, null),
            window: this._windowInfo(win),
        });
    }

    Activate(id) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        try {
            win.activate(global.get_current_time());
        } catch (e) {
            return JSON.stringify({ok: false, error: String(e), id});
        }
        return JSON.stringify({ok: true, id});
    }

    /// The window used before the focused one, for the macro `last` qualifier.
    LastWindow() {
        const focused = global.display.focus_window;
        for (const win of global.display.list_all_windows()) {
            if (win === focused || !this._isUserWindow(win))
                continue;
            if (safe(() => win.minimized, false))
                continue;
            return JSON.stringify({ok: true, window: this._windowInfo(win)});
        }
        return JSON.stringify({ok: true, window: null});
    }

    /// Move/resize in an explicit basis.
    ///
    /// `move_resize_frame` only accepts frame rectangles; `client` converts the
    /// request through the window's current margin. A maximized or minimized
    /// window ignores the write, so the state is dropped first and the write is
    /// deferred until the compositor has processed that change.
    MoveResize(id, x, y, w, h, basis) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        if (basis !== 'client' && basis !== 'frame')
            return JSON.stringify({ok: false, error: 'unknown_basis', basis});
        const wasMaximized = (maximized(win) ?? 0) !== 0;
        const wasMinimized = safe(() => win.minimized, false);
        if (wasMaximized)
            win.unmaximize(Meta.MaximizeFlags.BOTH);
        if (wasMinimized)
            win.unminimize();
        let error = null;
        const write = () => {
            if (isDestroyed(win))
                return GLib.SOURCE_REMOVE;
            const requested = {x, y, width: w, height: h};
            const frame = basis === 'frame' ? requested : clientToFrameRect(win, requested);
            try {
                win.move_resize_frame(true, frame.x, frame.y, frame.width, frame.height);
            } catch (e) {
                error = String(e);
            }
            return GLib.SOURCE_REMOVE;
        };
        const deferred = wasMaximized || wasMinimized;
        if (deferred)
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 150, write);
        else
            write();
        return JSON.stringify({
            ok: error === null,
            error,
            basis,
            requested: {x, y, w, h},
            unmaximized: wasMaximized,
            unminimized: wasMinimized,
            deferred,
            window: this._safeWindowInfo(win),
        });
    }

    SetDecorated(id, wanted) {
        const win = this._lookup(id);
        if (!win)
            return JSON.stringify({ok: false, error: 'no_such_window', id});
        // `decorated` is read-only on GNOME and windows always draw their own
        // decorations, so reporting success here would be a lie.
        return JSON.stringify({
            ok: false,
            error: 'unsupported',
            reason: 'gnome windows are always client-side decorated',
            requested: wanted,
            decorated: safe(() => win.decorated),
        });
    }

    // `get_keybinding_action` returns 0 when nothing is bound. The probe uses
    // the first configured accelerator and the evdev/us keycodes above.
    KeybindingState() {
        let accelerators = null;
        try {
            accelerators = this._settings.get_strv(KEYBINDING_NAME);
        } catch (e) {
            accelerators = [`error: ${e}`];
        }
        const probe = parseAccelerator(accelerators?.[0]);
        let actionId = -1;
        let actionError = null;
        if (probe) {
            try {
                actionId = global.display.get_keybinding_action(probe.keycode, probe.mask);
            } catch (e) {
                actionError = String(e);
            }
        } else {
            actionError = 'accelerator_not_parsed';
        }
        return JSON.stringify({
            keybinding_name: KEYBINDING_NAME,
            accelerators,
            probe,
            action_id: actionId,
            action_error: actionError,
            key_count: this._keyCount,
            last_key_time: this._lastKeyTime,
        });
    }

    /// Open the menu at `x, y`; a negative coordinate means the pointer, which
    /// lets `sizer menu` from the desktop entry work without an X connection.
    ShowMenu(x, y) {
        if (x < 0 || y < 0) {
            const pointer = safe(() => global.get_pointer(), null);
            [x, y] = pointer && pointer.length >= 2 ? pointer : this._focusAnchor();
        }
        return JSON.stringify(this._openMenu(x, y));
    }

    CloseMenu() {
        const wasOpen = this._menuSection?.isOpen ?? false;
        this._closeMenu();
        return JSON.stringify({ok: true, was_open: wasOpen});
    }

    /// Replace the menu entries the daemon pushes from the user configuration.
    ///
    /// Accepts the version 2 object `{presets, options}` and the older plain
    /// array. Everything the extension cannot honour is reported in `skipped`
    /// instead of being dropped silently.
    SetPresets(payload) {
        let raw = null;
        try {
            raw = JSON.parse(payload);
        } catch (e) {
            return JSON.stringify({ok: false, error: 'invalid_json', detail: String(e)});
        }
        const entries = Array.isArray(raw) ? raw : raw?.presets;
        if (!Array.isArray(entries))
            return JSON.stringify({ok: false, error: 'expected_presets_array'});
        const presets = [];
        const skipped = [];
        for (const entry of entries) {
            const preset = normalizePreset(entry);
            if (preset)
                presets.push(preset);
            else
                skipped.push({label: entry?.label ?? null, reason: presetProblem(entry)});
        }
        if (!presets.length)
            return JSON.stringify({ok: false, error: 'no_valid_presets', skipped});
        this._presets = presets;
        this._menuTree = menuTree(presets);
        this._presetLabels = presetLabels(this._menuTree);
        const hotkey = Array.isArray(raw) ? null : this._applyOptions(raw?.options);
        this._closeMenu();
        return JSON.stringify({
            ok: true,
            presets: presets.length,
            groups: countGroups(this._menuTree),
            skipped,
            options: this._options,
            hotkey,
        });
    }

    /// Adopt the pushed `area`/`basis` and bind the configured menu hotkey.
    ///
    /// A hotkey is only written when the configuration names one, so a user who
    /// customised the keybinding in GNOME Settings keeps it when the TOML says
    /// nothing about it.
    _applyOptions(raw) {
        const options = raw && typeof raw === 'object' ? raw : {};
        this._options = {
            area: options.area === 'monitor' ? 'monitor' : 'workarea',
            basis: options.basis === 'frame' ? 'frame' : 'client',
            allowOutsideBounds: options.allow_outside === true,
        };
        const hotkey = typeof options.hotkey === 'string' ? options.hotkey.trim() : '';
        if (!hotkey)
            return null;
        let applied = false;
        let error = null;
        try {
            const current = this._settings.get_strv(KEYBINDING_NAME);
            if (current.length !== 1 || current[0] !== hotkey) {
                this._settings.set_strv(KEYBINDING_NAME, [hotkey]);
                applied = true;
            }
        } catch (e) {
            error = String(e);
        }
        return {accelerator: hotkey, applied, error};
    }

    MenuState() {
        return JSON.stringify(this._menuState());
    }

    /// Activate one row through the same code path as a click.
    ///
    /// The reply is `{ok, activated, generation, pending, last_result?}`. A
    /// maximized window defers the frame write until the compositor drops that
    /// state, so `pending: true` means `last_result` is not there yet and
    /// `LastResult()` will carry it; otherwise `last_result` is included.
    ActivateMenuRow(index) {
        const row = Number(index);
        if (!this._menuSection || !this._menuItems.length)
            return JSON.stringify({ok: false, error: 'menu_closed'});
        if (!Number.isInteger(row) || row < 0 || row >= this._menuItems.length)
            return JSON.stringify({ok: false, error: 'no_such_row', index: row,
                items: this._menuItems.length});
        // `activate` runs `_choosePreset`, which closes the menu itself.
        this._menuItems[row].activate(null);
        const applied = this._lastApply ?? {generation: 0, pending: false};
        const reply = {ok: true, activated: row, generation: applied.generation,
            pending: applied.pending};
        if (!applied.pending)
            reply.last_result = this._lastResult;
        return JSON.stringify(reply);
    }

    LastResult() {
        return JSON.stringify(this._lastResult ?? {});
    }

    // ------------------------------------------------------------- menu core

    _openMenu(x, y) {
        this._closeMenu();
        if (!this._presets.length)
            return {ok: false, error: 'no_presets'};

        // A one-pixel anchor at the pointer gives the menu its origin. A real
        // PopupMenu brings the themed background and cascading submenus, and
        // the shell's menu manager grabs input and closes the menu on an
        // outside click or Escape, like every other menu.
        const anchor = new St.Widget({
            x,
            y,
            width: 1,
            height: 1,
            reactive: false,
            opacity: 0,
        });
        uiGroup().add_child(anchor);

        const monitor = monitorAt(global.display, x, y);
        const area = workArea(global.display, monitor)
            ?? {x: 0, y: 0, width: global.stage.width, height: global.stage.height};
        // Below the pointer normally, above it when the bottom half is short.
        const side = y - area.y > area.y + area.height - y ? St.Side.BOTTOM : St.Side.TOP;
        const menu = new PopupMenu.PopupMenu(anchor, 0.0, side);

        const items = [];
        const addNodes = (parent, nodes) => {
            for (const node of nodes) {
                if (node.kind === 'preset') {
                    const item = new PopupMenu.PopupMenuItem(node.label);
                    item.connect('activate', () => this._choosePreset(this._presets[node.index]));
                    parent.addMenuItem(item);
                    // Depth-first order, the order `ActivateMenuRow` indexes.
                    items.push(item);
                } else if (node.kind === 'separator') {
                    parent.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
                } else {
                    const submenu = new PopupMenu.PopupSubMenuMenuItem(node.label);
                    addNodes(submenu.menu, node.children);
                    parent.addMenuItem(submenu);
                }
            }
        };
        addNodes(menu, this._menuTree);
        uiGroup().add_child(menu.actor);
        Main.panel.menuManager.addMenu(menu);
        const openStateId = menu.connect('open-state-changed', (_menu, open) => {
            if (!open) {
                // The manager closed the menu (outside click, Escape, another
                // menu opening): drop our actors and the manager entry.
                menu.disconnect(openStateId);
                this._closeMenu();
            }
        });
        menu.open();

        const [, , width, height] = menu.actor.get_preferred_size();
        this._menuSection = menu;
        this._menuAnchor = anchor;
        this._menuItems = items;
        // The pointer the menu was opened at; the box pointer keeps the menu
        // inside the work area around it.
        this._menuGeometry = {x, y, w: width, h: height};
        return {...this._menuState(), ok: true};
    }

    _menuState() {
        return {
            open: this._menuSection?.isOpen ?? false,
            // A separator has no row index, so it is left out here: every entry
            // keeps pointing at the same `ActivateMenuRow` index.
            items: this._menuTree
                .filter(node => node.kind !== 'separator')
                .map(node => node.label),
            tree: this._menuTree,
            geometry: this._menuGeometry,
            options: this._options,
            presets: this._presetLabels,
        };
    }

    _closeMenu() {
        if (!this._menuSection)
            return;
        const menu = this._menuSection;
        const anchor = this._menuAnchor;
        this._menuSection = null;
        this._menuAnchor = null;
        this._menuItems = [];
        this._menuGeometry = null;
        // Destroying the menu also releases the menu manager grab.
        menu.destroy();
        anchor?.destroy();
    }

    /// Apply a menu row to the window that had focus when the menu opened.
    _choosePreset(preset) {
        const win = this._targetWindow();
        this._closeMenu();
        if (!win) {
            this._lastResult = {label: preset.label, error: 'no_target_window', exact: false};
            this._lastApply = {generation: 0, pending: false};
            this._emitChanged('preset-failed', this._lastResult);
            return this._lastApply;
        }
        return this._applyPreset(win, preset);
    }

    _targetWindow() {
        const focused = global.display.focus_window;
        if (focused && this._isUserWindow(focused))
            return focused;
        for (const win of global.display.list_all_windows()) {
            if (this._isUserWindow(win) && !safe(() => win.minimized, false))
                return win;
        }
        return null;
    }

    /// Resolve a preset to a frame rect and write it with `move_resize_frame`.
    ///
    /// `monitor`, the global area kind and the size basis are the same fields
    /// the X11 backend honours, so one TOML behaves the same on both platforms.
    ///
    /// Returns `{generation, pending}`. `pending` means the window was maximized
    /// and the write waits for the compositor to drop that state; `LastResult`
    /// carries the outcome once it happened.
    _applyPreset(win, preset) {
        const request = this._presetQueue.begin(win);
        const options = this._options
            ?? {area: 'workarea', basis: 'client', allowOutsideBounds: false};
        const monitor = this._resolveMonitor(preset.monitor);
        if (monitor === null) {
            this._lastResult = {
                label: preset.label,
                generation: request.generation,
                error: 'no_such_monitor',
                requested_monitor: preset.monitor,
                exact: false,
            };
            this._emitChanged('preset-failed', this._lastResult);
            this._lastApply = {generation: request.generation, pending: false};
            return this._lastApply;
        }
        const area = options.area === 'monitor'
            ? safe(() => global.display.get_monitor_geometry(monitor), null)
            : workArea(global.display, monitor);
        if (!area) {
            this._lastResult = {label: preset.label, error: 'no_work_area', exact: false};
            this._emitChanged('preset-failed', this._lastResult);
            this._lastApply = {generation: request.generation, pending: false};
            return this._lastApply;
        }
        const basis = options.basis === 'frame' ? 'frame' : 'client';
        const allowOutside = preset.allowOutsideBounds ?? options.allowOutsideBounds === true;
        // A move-only preset keeps the size the window has right now.
        const currentSize = safe(() => {
            const rect = win.get_frame_rect();
            return basis === 'frame' ? {w: rect.width, h: rect.height}
                : {w: win.frame_rect_to_client_rect(rect).width,
                   h: win.frame_rect_to_client_rect(rect).height};
        }, null);
        const size = preset.size ? resolveSize(preset.size, area) : currentSize;
        const wasMaximized = (maximized(win) ?? 0) !== 0;
        let error = null;
        const write = () => {
            if (!this._presetQueue.isCurrent(request) || isDestroyed(win))
                return false;
            // Geometry is read here: the client <-> frame offset comes from the
            // window's current state, and only matches the written rect once the
            // compositor has dropped the maximized state.
            const frameRect = safe(() => win.get_frame_rect(), null);
            const current = basis === 'frame'
                ? frameRect
                : safe(() => win.frame_rect_to_client_rect(frameRect), null);
            // An absolute position is relative to the area origin and is written
            // verbatim; without one the window keeps its current place.
            let origin = preset.position
                ? {x: area.x + preset.position[0], y: area.y + preset.position[1]}
                : preset.anchor
                    ? anchorPosition(area, size.w, size.h, preset.anchor)
                    : {x: current?.x ?? area.x, y: current?.y ?? area.y};
            if (!allowOutside)
                origin = clampOrigin(origin, size, area);
            const target = {x: origin.x, y: origin.y, width: size.w, height: size.h};
            const frame = basis === 'frame' ? target : clientToFrameRect(win, target);
            const requestedBasis = {x: target.x, y: target.y, w: target.width, h: target.height};
            try {
                win.move_resize_frame(true, frame.x, frame.y, frame.width, frame.height);
            } catch (e) {
                error = String(e);
            }
            const immediate = this._safeWindowInfo(win);
            this._lastResult = {
                label: preset.label,
                generation: request.generation,
                basis,
                monitor,
                area: {x: area.x, y: area.y, w: area.width, h: area.height},
                outside: !withinArea({x: target.x, y: target.y, w: target.width, h: target.height},
                    area),
                allow_outside: allowOutside,
                requested_basis: requestedBasis,
                requested_client: basis === 'frame'
                    ? safe(() => win.frame_rect_to_client_rect(frame), requestedBasis)
                    : requestedBasis,
                requested_frame: {x: frame.x, y: frame.y, w: frame.width, h: frame.height},
                achieved_client: immediate?.client ?? null,
                achieved_frame: immediate?.frame ?? null,
                unmaximized: wasMaximized,
                exact: null,
                error,
            };
            this._emitChanged('preset-applied', this._lastResult);
            this._verifyPreset(win, request, this._lastResult);
            return true;
        };

        this._lastApply = {generation: request.generation, pending: false};
        // A maximized window is the obvious case, but a second preset picked while
        // the first one's unmaximize is still in flight sees the maximized flag
        // already cleared and would write against an unsettled frame offset.
        if (!wasMaximized && !this._presetQueue.waiting(win)) {
            write();
            return this._lastApply;
        }

        // A frame write in the same turn as the unmaximize is ignored by mutter, and
        // a fixed delay only guesses when that turn ends. `size-changed` is the
        // window reporting the geometry the compositor restored, i.e. the first
        // moment the offset and the write are both meaningful; the timeout only
        // covers a window that unmaximizes without changing size. The queue's
        // per-window generation is what makes a newer request win.
        const ids = [];
        let timer = 0;
        let settleTimer = 0;
        let settleStarted = false;
        let done = false;
        // Ending the wait without a write has to leave a result: otherwise the
        // request disappears and `LastResult()` keeps showing the previous preset,
        // which reads as "the key press did nothing".
        const abort = (reason) => {
            this._lastResult = {label: preset.label, generation: request.generation,
                ok: false, exact: false, reason, superseded: reason === 'superseded'};
            if (this._lastApply?.generation === request.generation)
                this._lastApply = {generation: request.generation, pending: false};
            this._emitChanged('preset-failed', this._lastResult);
        };
        const detach = (reason) => {
            if (done)
                return;
            done = true;
            for (const id of ids.splice(0))
                safe(() => win.disconnect(id));
            if (timer) {
                GLib.source_remove(timer);
                timer = 0;
            }
            if (settleTimer) {
                GLib.source_remove(settleTimer);
                settleTimer = 0;
            }
            this._presetQueue.settled(request);
            if (reason)
                abort(reason);
        };
        const connect = (signal, handler) => {
            const id = safe(() => win.connect(signal, handler), null);
            if (id !== null)
                ids.push(id);
        };
        // Convert only once the compositor has settled the window. Right after the
        // unmaximize the reported shadow margin is not the settled one: GNOME 50
        // reports the maximized margin (zero) for 60-110ms (measured), and 46 goes
        // through an intermediate margin while the restore animates. A client rect
        // converted then is off by that difference (measured: a 600x400 request
        // landed as 586x388 and 388x289). So the write waits until the frame rect
        // and the margin are unchanged for three samples, and the margin is real; a
        // window that states it has no client-side decorations only needs the
        // geometry to hold still. GNOME 50 removed `is_client_decorated` (unknown),
        // and unknown must not be read as "undecorated". The wait is bounded so an
        // unreadable window cannot stall the write.
        let settlePrev = null;
        let settleCount = 0;
        const scheduleSettle = (attempt) => {
            settleTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 32, () => {
                settleTimer = 0;
                writeOnceSettled(attempt + 1);
                return GLib.SOURCE_REMOVE;
            });
        };
        const writeOnceSettled = (attempt) => {
            settleTimer = 0;
            settleStarted = true;
            if (!this._presetQueue.isCurrent(request)) {
                detach('superseded');
                return;
            }
            if (isDestroyed(win)) {
                detach('window destroyed');
                return;
            }
            // Not a live path: instrumented runs on 50.1 (two runs, 8 writes) never
            // saw `size-changed` arrive while the maximum was still set, and the
            // case passes with a plain return here. The one second timer armed for
            // this request is what reports `unmaximize_timeout` when the maximum
            // really persists, so this branch only has to avoid writing against a
            // maximized frame.
            if ((maximized(win) ?? 0) !== 0)
                return;
            const info = this._safeWindowInfo(win);
            const frame = info?.frame ?? null;
            const client = info?.client ?? null;
            if (!frame || !client) {
                detach();
                write();
                return;
            }
            const sample = {frame, margin: {dx: client.x - frame.x, dy: client.y - frame.y,
                dw: client.w - frame.w, dh: client.h - frame.h}};
            const held = settlePrev !== null && sameRect(settlePrev.frame, frame) &&
                settlePrev.margin.dx === sample.margin.dx &&
                settlePrev.margin.dy === sample.margin.dy &&
                settlePrev.margin.dw === sample.margin.dw &&
                settlePrev.margin.dh === sample.margin.dh;
            settlePrev = sample;
            settleCount = held ? settleCount + 1 : 0;
            const noMargin = clientDecorated(win) === false;
            const hasMargin = sample.margin.dx !== 0 || sample.margin.dy !== 0 ||
                sample.margin.dw !== 0 || sample.margin.dh !== 0;
            const ready = settleCount >= 3 && (noMargin || hasMargin);
            if (!ready && attempt < 28) {
                scheduleSettle(attempt);
                return;
            }
            detach();
            write();
        };
        const finish = () => {
            if (!this._presetQueue.isCurrent(request)) {
                detach('superseded');
                return;
            }
            if (isDestroyed(win)) {
                detach('window destroyed');
                return;
            }
            writeOnceSettled(0);
        };
        connect('size-changed', finish);
        connect('unmanaged', () => detach('window destroyed'));
        timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
            timer = 0;
            if (!this._presetQueue.isCurrent(request) || isDestroyed(win)) {
                detach(isDestroyed(win) ? 'window destroyed' : 'superseded');
                return GLib.SOURCE_REMOVE;
            }
            if ((maximized(win) ?? 0) !== 0) {
                // A second passed without the maximum going away. Writing now would
                // be dropped by mutter, so this reports a failure instead of a
                // success that never reaches the window.
                detach('unmaximize_timeout');
                return GLib.SOURCE_REMOVE;
            }
            // No size-changed arrived (or the settle loop is still running): start the
            // settle path, which writes on its own schedule. Calling it while it is
            // already running would restart its sample history, so it is skipped.
            if (!settleStarted)
                writeOnceSettled(0);
            return GLib.SOURCE_REMOVE;
        });
        this._presetQueue.defer(request, detach);
        this._lastApply = {generation: request.generation, pending: true};
        if (wasMaximized)
            win.unmaximize(Meta.MaximizeFlags.BOTH);
        return this._lastApply;
    }

    /// Monitor index for a selector, or null when the index does not exist.
    _resolveMonitor(selector) {
        if (selector === null || selector === undefined || selector === 'active') {
            const pointer = safe(() => global.get_pointer(), null);
            return pointer && pointer.length >= 2
                ? monitorAt(global.display, pointer[0], pointer[1])
                : safe(() => global.display.get_current_monitor(), 0);
        }
        if (selector === 'primary')
            return safe(() => global.display.get_primary_monitor(), 0);
        const count = safe(() => global.display.get_n_monitors(), 0);
        return selector >= 0 && selector < count ? selector : null;
    }

    /// Fill in `exact` once the compositor has applied the request.
    ///
    /// `generation` keeps a sample that belongs to a superseded request from
    /// overwriting a newer result, and a destroyed window is skipped entirely:
    /// reading geometry from one raises a GLib critical. The result object is
    /// captured here instead of read back from `_lastResult`, so a write on
    /// another window in between cannot redirect this verification.
    _verifyPreset(win, request, result) {
        const id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 150, () => {
            this._verifyTimers.delete(id);
            if (!this._presetQueue.isCurrent(request) || isDestroyed(win))
                return GLib.SOURCE_REMOVE;
            const info = this._safeWindowInfo(win);
            if (!info)
                return GLib.SOURCE_REMOVE;
            result.achieved_client = info.client;
            result.achieved_frame = info.frame;
            const achieved = result.basis === 'frame' ? info.frame : info.client;
            const requested = result.requested_basis;
            result.exact = achieved !== null && requested !== null &&
                achieved.x === requested.x &&
                achieved.y === requested.y &&
                achieved.w === requested.w &&
                achieved.h === requested.h;
            this._emitChanged('preset-verified', result);
            return GLib.SOURCE_REMOVE;
        });
        this._verifyTimers.add(id);
    }

    _safeWindowInfo(win) {
        return safe(() => this._windowInfo(win), null);
    }

    _focusAnchor() {
        const frame = safe(() => {
            const win = global.display.focus_window;
            return win ? win.get_frame_rect() : null;
        }, null);
        if (!frame)
            return [0, 0];
        return [Math.round(frame.x + frame.width / 2), Math.round(frame.y + frame.height / 2)];
    }

    // ------------------------------------------------------------- internals

    _isUserWindow(win) {
        if (!win)
            return false;
        if (safe(() => win.is_override_redirect(), false) || safe(() => win.is_skip_taskbar(), false))
            return false;
        const type = safe(() => win.get_window_type());
        return type === Meta.WindowType.NORMAL ||
            type === Meta.WindowType.DIALOG ||
            type === Meta.WindowType.MODAL_DIALOG ||
            type === Meta.WindowType.UTILITY ||
            type === Meta.WindowType.TOOLBAR;
    }

    _listWindows() {
        const result = [];
        for (const win of global.display.list_all_windows()) {
            if (!this._isUserWindow(win))
                continue;
            result.push(this._windowInfo(win));
        }
        return result;
    }

    _lookup(id) {
        const wanted = String(id);
        for (const win of global.display.list_all_windows()) {
            if (String(win.get_id()) === wanted)
                return win;
        }
        return null;
    }

    _windowInfo(win) {
        const frame = win.get_frame_rect();
        const buffer = safe(() => win.get_buffer_rect());
        const client = safe(() => win.frame_rect_to_client_rect(frame));
        const monitor = safe(() => win.get_monitor(), 0);
        const workArea = safe(() => win.get_work_area_for_monitor(monitor));
        return {
            id: String(win.get_id()),
            stable_sequence: safe(() => win.get_stable_sequence()),
            title: safe(() => win.get_title(), ''),
            wm_class: safe(() => win.get_wm_class()),
            wm_class_instance: safe(() => win.get_wm_class_instance()),
            pid: safe(() => win.get_pid()),
            type: safe(() => win.get_window_type()),
            frame: rectToObj(frame),
            buffer: rectToObj(buffer),
            client: rectToObj(client),
            above: safe(() => win.is_above(), false),
            decorated: safe(() => win.decorated),
            client_decorated: clientDecorated(win),
            maximized: maximized(win),
            minimized: safe(() => win.minimized, false),
            focused: safe(() => win.has_focus(), false),
            monitor,
            monitor_scale: safe(() => global.display.get_monitor_scale(monitor), 1),
            work_area: rectToObj(workArea),
            on_all_workspaces: safe(() => win.is_on_all_workspaces(), false),
            // `-1` rather than a guess when the workspace is not available.
            workspace: safe(() => win.get_workspace().index(), -1),
        };
    }

    _registerKeybinding() {
        try {
            Main.wm.addKeybinding(
                KEYBINDING_NAME,
                this._settings,
                Meta.KeyBindingFlags.NONE,
                Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW | Shell.ActionMode.POPUP,
                () => {
                    this._keyCount++;
                    this._lastKeyTime = GLib.get_monotonic_time();
                    if (this._menuSection?.isOpen) {
                        this._closeMenu();
                        this._emitChanged('menu-closed');
                        return;
                    }
                    const pointer = safe(() => global.get_pointer(), null);
                    const anchor = pointer && pointer.length >= 2 ? pointer : this._focusAnchor();
                    this._openMenu(anchor[0], anchor[1]);
                    this._emitChanged('menu-opened');
                });
        } catch (e) {
            console.error(`[sizer] addKeybinding failed: ${e}`);
        }
    }

    _notifyWindows() {
        for (const win of global.display.list_all_windows()) {
            if (!this._isUserWindow(win))
                continue;
            const id = win.get_id();
            if (this._connectedWindows.has(id))
                continue;
            const sizeId = win.connect('size-changed', () => this._geometryChanged());
            const posId = win.connect('position-changed', () => this._geometryChanged());
            // `unmanaged` fires when the window leaves the display: without it
            // the map and its signal connections would grow for the whole
            // session because nothing ever removed an entry.
            const unmanagedId = win.connect('unmanaged', () => {
                const entry = this._connectedWindows.get(id);
                if (!entry)
                    return;
                this._connectedWindows.delete(id);
                try {
                    win.disconnect(entry.sizeId);
                    win.disconnect(entry.posId);
                } catch (e) {
                    // the window is already being torn down
                }
                this._emitChanged('window-destroyed');
            });
            this._connectedWindows.set(id, {win, sizeId, posId, unmanagedId});
        }
    }

    /// Coalesce geometry changes into one broadcast per 150ms.
    ///
    /// Resizing emits size-changed and position-changed for every intermediate
    /// frame, and each broadcast serializes the whole window list; doing that
    /// per frame stalls the compositor during exactly the drag Sizer is for.
    _geometryChanged() {
        if (this._geometryTimer)
            return;
        this._geometryTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 150, () => {
            this._geometryTimer = 0;
            this._emitChanged('window-geometry');
            return GLib.SOURCE_REMOVE;
        });
    }

    _disconnectWindows() {
        for (const {win, sizeId, posId, unmanagedId} of this._connectedWindows.values()) {
            try {
                win.disconnect(sizeId);
                win.disconnect(posId);
                if (unmanagedId)
                    win.disconnect(unmanagedId);
            } catch (e) {
                // window already gone
            }
        }
        this._connectedWindows.clear();
        if (this._geometryTimer) {
            GLib.source_remove(this._geometryTimer);
            this._geometryTimer = 0;
        }
    }
}
