//! X11 window backend (EWMH-based).
//!
//! Two Phase 0 findings are structural here and must not be "simplified" away:
//!
//! 1. Geometry is written **only** through `_NET_MOVERESIZE_WINDOW` with
//!    StaticGravity and source=pager. Raw `ConfigureWindow` works once and is
//!    then silently ignored by mutter, which looks like success.
//! 2. A successful `XGrabKey` does not prove the hotkey works. Registration
//!    reports [`HotkeyOutcome::RegisteredUnverified`] unless a delivery probe
//!    actually observed the injected combination.

mod atoms;
mod keys;

use std::thread::sleep;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;

use sizer_core::{
    AreaKind, Extents, Geometry, GeometryResult, MonitorSelector, Rect, SizeBasis, WindowId,
    WindowInfo,
};
use sizer_platform::{
    BackendError, BackendResult, Behavior, Capabilities, HotkeyBackend, HotkeyOutcome,
    HotkeyReport, HotkeySpec, Monitor, WindowBackend,
};

use keys::GrabFailure;

/// Parse an accelerator string without touching a display.
///
/// `sizer config check` uses this so a typo in `hotkey = ...` is reported before
/// the user finds out at registration time.
pub fn validate_accelerator(accelerator: &str) -> Result<(), String> {
    keys::parse_accelerator(accelerator)
        .map(|_| ())
        .map_err(|error| match error {
            BackendError::Protocol(message) => message,
            other => other.to_string(),
        })
}

/// Modifier state of a key event with the bits a grab cannot express removed.
///
/// [`keys::grab`] grabs the num-lock and caps-lock variants of each
/// combination, so those bits say nothing about which hotkey the user pressed.
/// Pointer button bits also arrive in the key event state while a button is
/// held; keeping them would make that keystroke stop matching its hotkey.
pub fn hotkey_modifiers(state: u16) -> u16 {
    state & !(keys::MOD_LOCK | keys::MOD_MOD2 | keys::MOD_BUTTONS)
}

/// Time allowed for the window manager to apply a change and settle.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Time allowed for a state change (maximize / above / decorations) to appear.
const STATE_TIMEOUT: Duration = Duration::from_millis(1200);
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// Client minimum and maximum size from `WM_NORMAL_HINTS`.
type SizeHintLimits = ((i32, i32), (i32, i32));

/// Whether hotkey registration performs an XTEST delivery probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbePolicy {
    /// Never inject input. Registration is reported as unverified.
    Off,
    /// Inject the combination once and see whether the grab receives it.
    Inject,
}

pub struct X11Backend {
    conn: RustConnection,
    root: Window,
    atoms: atoms::Atoms,
    non_controllable_types: Vec<u32>,
    probe: ProbePolicy,
}

impl X11Backend {
    pub fn connect() -> BackendResult<Self> {
        Self::connect_with(ProbePolicy::Off)
    }

    pub fn connect_with(probe: ProbePolicy) -> BackendResult<Self> {
        if std::env::var("DISPLAY").is_ok_and(|display| display.trim().is_empty()) {
            return Err(BackendError::Unavailable(
                "DISPLAY is empty; set it to an X11 display such as :0, or run under Xvfb"
                    .to_string(),
            ));
        }
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|err| BackendError::Unavailable(format!("cannot open X display: {err}")))?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = atoms::Atoms::intern(&conn)?;
        let non_controllable_types = atoms::NON_CONTROLLABLE_TYPES
            .iter()
            .map(|name| atoms::intern_type(&conn, name))
            .collect::<BackendResult<Vec<_>>>()?;
        Ok(Self {
            conn,
            root,
            atoms,
            non_controllable_types,
            probe,
        })
    }

    fn parse_window(id: &WindowId) -> BackendResult<Window> {
        id.0.parse::<u32>().map_err(|_| {
            BackendError::WindowNotFound(format!("{:?} is not an X11 window id", id.0))
        })
    }

    fn ensure_window(&self, win: Window) -> BackendResult<()> {
        let missing = || BackendError::WindowNotFound(format!("X11 window {win} does not exist"));
        self.conn
            .get_window_attributes(win)
            .map_err(|_| missing())?
            .reply()
            .map(|_| ())
            .map_err(|_| missing())
    }

    fn prop_u32(&self, win: Window, atom: u32) -> BackendResult<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, 4096)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        Ok(reply
            .value32()
            .map(|values| values.collect())
            .unwrap_or_default())
    }

    fn prop_string(&self, win: Window, atom: u32) -> BackendResult<String> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, 4096)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        Ok(String::from_utf8_lossy(&reply.value)
            .trim_end_matches('\0')
            .to_string())
    }

    fn wm_class(&self, win: Window) -> BackendResult<Option<String>> {
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        let parts: Vec<&[u8]> = reply.value.split(|byte| *byte == 0).collect();
        // WM_CLASS is instance\0class\0; the class part is the stable identifier.
        let class = parts
            .get(1)
            .filter(|part| !part.is_empty())
            .or(parts.first());
        Ok(class.map(|bytes| String::from_utf8_lossy(bytes).to_string()))
    }

    fn frame_extents(&self, win: Window) -> BackendResult<Extents> {
        let values = self.prop_u32(win, self.atoms.frame_extents)?;
        // EWMH order is left, right, top, bottom.
        Ok(match values.as_slice() {
            [left, right, top, bottom] => Extents {
                left: *left as i32,
                top: *top as i32,
                right: *right as i32,
                bottom: *bottom as i32,
            },
            _ => Extents::ZERO,
        })
    }

    fn maximized(&self, win: Window) -> BackendResult<bool> {
        let states = self.prop_u32(win, self.atoms.wm_state)?;
        Ok(states.contains(&self.atoms.state_maximized_vert)
            || states.contains(&self.atoms.state_maximized_horz))
    }

    fn hidden(&self, win: Window) -> BackendResult<bool> {
        Ok(self
            .prop_u32(win, self.atoms.wm_state)?
            .contains(&self.atoms.state_hidden))
    }

    fn is_above(&self, win: Window) -> BackendResult<bool> {
        Ok(self
            .prop_u32(win, self.atoms.wm_state)?
            .contains(&self.atoms.state_above))
    }

    fn work_area_rect(&self) -> BackendResult<Option<Rect>> {
        let workarea = self.prop_u32(self.root, self.atoms.workarea)?;
        let desktop = self.current_desktop()? as usize;
        let offset = desktop * 4;
        Ok(workarea.get(offset..offset + 4).map(|slice| {
            Rect::new(
                slice[0] as i32,
                slice[1] as i32,
                slice[2] as i32,
                slice[3] as i32,
            )
        }))
    }

    fn atom_name(&self, atom: u32) -> BackendResult<String> {
        let reply = self
            .conn
            .get_atom_name(atom)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        Ok(String::from_utf8_lossy(&reply.name).to_string())
    }

    fn screen_monitor(&self) -> BackendResult<Monitor> {
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        Ok(Monitor {
            name: "screen".to_string(),
            rect: Rect::new(0, 0, geometry.width as i32, geometry.height as i32),
            primary: true,
        })
    }

    fn is_controllable(&self, win: Window) -> BackendResult<bool> {
        if self.wm_class(win)?.as_deref() == Some("mutter-x11-frames") {
            return Ok(false);
        }
        match self
            .conn
            .get_window_attributes(win)
            .map(|cookie| cookie.reply())
        {
            Ok(Ok(attrs)) if attrs.override_redirect => return Ok(false),
            Ok(Ok(_)) => {}
            // Destroyed between the property read and now.
            _ => return Ok(false),
        }
        let types = self.prop_u32(win, self.atoms.wm_window_type)?;
        Ok(!types
            .iter()
            .any(|value| self.non_controllable_types.contains(value)))
    }

    fn window_info(&self, win: Window, focused: bool) -> BackendResult<WindowInfo> {
        let geometry = self.geometry(&WindowId(win.to_string()))?;
        let net_name = self.prop_string(win, self.atoms.wm_name)?;
        let title = if net_name.is_empty() {
            self.prop_string(win, u32::from(AtomEnum::WM_NAME))?
        } else {
            net_name
        };
        Ok(WindowInfo {
            id: WindowId(win.to_string()),
            title,
            app_id: self.wm_class(win)?,
            pid: self.prop_u32(win, self.atoms.wm_pid)?.first().copied(),
            geometry,
            maximized: self.maximized(win)?,
            above: self.is_above(win)?,
            focused,
        })
    }

    fn send_client_message(&self, win: Window, type_: u32, data: [u32; 5]) -> BackendResult<()> {
        let event = ClientMessageEvent {
            response_type: 33,
            format: 32,
            sequence: 0,
            window: win,
            type_,
            data: ClientMessageData::from(data),
        };
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        self.conn
            .flush()
            .map_err(|err| BackendError::Protocol(err.to_string()))
    }

    fn set_wm_state(&self, win: Window, action: u32, first: u32, second: u32) -> BackendResult<()> {
        // Source indication 1 = normal application.
        self.send_client_message(win, self.atoms.wm_state, [action, first, second, 1, 0])
    }

    /// Ask the window manager to move/resize. `client` is in root coordinates.
    fn send_moveresize(&self, win: Window, client: Rect) -> BackendResult<()> {
        // gravity=StaticGravity(10), bits 8..=11 mark x/y/w/h as present, source=2 (pager).
        let flags: u32 = 10 | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (2 << 12);
        self.send_client_message(
            win,
            self.atoms.moveresize_window,
            [
                flags,
                client.x as u32,
                client.y as u32,
                client.w.max(1) as u32,
                client.h.max(1) as u32,
            ],
        )
    }

    fn wait_until<F: Fn() -> bool>(&self, timeout: Duration, predicate: F) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep(POLL_INTERVAL);
        }
    }

    /// Wait for the geometry to change away from `baseline` and then hold still.
    fn wait_for_geometry(
        &self,
        win: Window,
        baseline: Rect,
        timeout: Duration,
    ) -> BackendResult<(Geometry, bool)> {
        let deadline = Instant::now() + timeout;
        let mut previous = self.geometry(&WindowId(win.to_string()))?;
        let mut changed = false;
        loop {
            sleep(POLL_INTERVAL);
            let current = self.geometry(&WindowId(win.to_string()))?;
            if current.client != baseline {
                changed = true;
            }
            if changed && current.client == previous.client {
                return Ok((current, changed));
            }
            previous = current;
            if Instant::now() >= deadline {
                return Ok((previous, changed));
            }
        }
    }

    /// Client minimum and maximum size from `WM_NORMAL_HINTS`, when the window
    /// sets any. ICCCM stores zero for "unset"; hints are client dimensions.
    fn size_hint_limits(&self, win: Window) -> BackendResult<Option<SizeHintLimits>> {
        let values = self.prop_u32(win, self.atoms.normal_hints)?;
        if values.len() < 9 {
            return Ok(None);
        }
        Ok(Some((
            (values[5] as i32, values[6] as i32),
            (values[7] as i32, values[8] as i32),
        )))
    }

    /// Wait until two consecutive reads agree, e.g. after leaving a maximized state.
    fn settle(&self, win: Window, timeout: Duration) -> BackendResult<Geometry> {
        let deadline = Instant::now() + timeout;
        let mut previous = self.geometry(&WindowId(win.to_string()))?;
        loop {
            sleep(POLL_INTERVAL);
            let current = self.geometry(&WindowId(win.to_string()))?;
            if current.client == previous.client || Instant::now() >= deadline {
                return Ok(current);
            }
            previous = current;
        }
    }

    /// Consume key presses delivered to this client for `duration`.
    ///
    /// Only useful while a grab is held by this process; the daemon uses it to
    /// stay alive after registering hotkeys.
    pub fn watch_keypresses(&self, duration: Duration) -> BackendResult<Vec<u8>> {
        let deadline = Instant::now() + duration;
        let mut pressed = Vec::new();
        while Instant::now() < deadline {
            match self.next_keypress(deadline.saturating_duration_since(Instant::now()))? {
                Some((keycode, _)) => pressed.push(keycode),
                None => break,
            }
        }
        Ok(pressed)
    }

    /// Wait up to `timeout` for the next key press delivered to this client.
    ///
    /// Returns the keycode with the modifier state that came with the event:
    /// two hotkeys can share a keycode and differ only in modifiers, so the
    /// keycode alone cannot identify which one fired.
    pub fn next_keypress(&self, timeout: Duration) -> BackendResult<Option<(u8, u16)>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(x11rb::protocol::Event::KeyPress(press))) => {
                    return Ok(Some((press.detail, u16::from(press.state))));
                }
                Ok(Some(_)) => {}
                Ok(None) => sleep(POLL_INTERVAL),
                Err(err) => return Err(BackendError::Protocol(err.to_string())),
            }
        }
        Ok(None)
    }

    /// Keycode and modifier mask of an accelerator in the current keyboard map.
    pub fn keycode_and_mask_for(&self, accelerator: &str) -> BackendResult<(u8, u16)> {
        let parsed = keys::parse_accelerator(accelerator)?;
        let keycode = keys::keycode_for(&self.conn, parsed.keysym).ok_or_else(|| {
            BackendError::Protocol(format!(
                "keysym {:#x} is not in the keyboard map",
                parsed.keysym
            ))
        })?;
        Ok((keycode, parsed.mask))
    }

    /// Add or remove the EWMH maximized states and confirm the result.
    pub fn set_maximized(&self, id: &WindowId, maximized: bool) -> BackendResult<bool> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        self.set_wm_state(
            win,
            u32::from(maximized),
            self.atoms.state_maximized_vert,
            self.atoms.state_maximized_horz,
        )?;
        if !self.wait_until(STATE_TIMEOUT, || self.maximized(win).unwrap_or(!maximized)) {
            return Err(BackendError::Protocol(format!(
                "window manager did not turn maximization {}",
                if maximized { "on" } else { "off" }
            )));
        }
        self.maximized(win)
    }

    fn pointer_position(&self) -> BackendResult<(i32, i32)> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    /// Pointer position in root coordinates.
    pub fn pointer(&self) -> BackendResult<(i32, i32)> {
        self.pointer_position()
    }

    /// `_NET_CURRENT_DESKTOP`; 0 when the window manager publishes none.
    pub fn current_desktop(&self) -> BackendResult<u32> {
        Ok(self
            .prop_u32(self.root, self.atoms.current_desktop)?
            .first()
            .copied()
            .unwrap_or(0))
    }

    /// `_NET_WM_DESKTOP` of one window; `u32::MAX` means "on every desktop".
    pub fn window_desktop(&self, id: &WindowId) -> BackendResult<u32> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        Ok(self
            .prop_u32(win, self.atoms.wm_desktop)?
            .first()
            .copied()
            .unwrap_or(0))
    }

    /// Controllable windows from top to bottom of the stacking order.
    pub fn stacking_order(&self) -> BackendResult<Vec<WindowId>> {
        let mut windows = Vec::new();
        for win in self
            .prop_u32(self.root, self.atoms.client_list_stacking)?
            .into_iter()
            .rev()
        {
            if self.is_controllable(win)? {
                windows.push(WindowId(win.to_string()));
            }
        }
        Ok(windows)
    }

    /// Whether the window manager currently reports the window as iconified.
    pub fn is_minimized(&self, id: &WindowId) -> BackendResult<bool> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        self.hidden(win)
    }

    /// Focus `id` through `_NET_ACTIVE_WINDOW`, which also restores it.
    pub fn activate_window(&self, id: &WindowId) -> BackendResult<()> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        // Source 2 = pager, so the window manager does not apply focus stealing
        // prevention to something the user asked a tool to do.
        self.send_client_message(win, self.atoms.active_window, [2, 0, 0, 0, 0])?;
        self.wait_until(STATE_TIMEOUT, || !self.hidden(win).unwrap_or(true));
        Ok(())
    }

    /// Iconify or restore a window and confirm the hidden state flipped.
    pub fn set_minimized(&self, id: &WindowId, minimized: bool) -> BackendResult<bool> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        if minimized {
            // ICCCM: WM_CHANGE_STATE with IconicState (3).
            self.send_client_message(win, self.atoms.wm_change_state, [3, 0, 0, 0, 0])?;
        } else {
            self.send_client_message(win, self.atoms.active_window, [2, 0, 0, 0, 0])?;
        }
        if !self.wait_until(STATE_TIMEOUT, || self.hidden(win).ok() == Some(minimized)) {
            return Err(BackendError::Protocol(format!(
                "window manager did not turn the minimized state {}",
                if minimized { "on" } else { "off" }
            )));
        }
        self.hidden(win)
    }

    /// Reference area around `point`: the monitor under it, minus panels for
    /// [`AreaKind::WorkArea`]. Used to place the cursor-anchored menu.
    pub fn area_at(&self, point: (i32, i32), kind: AreaKind) -> BackendResult<Rect> {
        let monitors = self.monitors()?;
        let probe = Rect::new(point.0, point.1, 1, 1);
        let rect = monitors
            .iter()
            .find(|monitor| monitor.rect.contains(&probe))
            .or_else(|| monitors.iter().find(|monitor| monitor.primary))
            .or_else(|| monitors.first())
            .map(|monitor| monitor.rect)
            .ok_or_else(|| BackendError::Unavailable("no monitors reported".to_string()))?;
        Ok(match kind {
            AreaKind::Monitor => rect,
            AreaKind::WorkArea => self
                .work_area_rect()?
                .and_then(|work_area| intersect(rect, work_area))
                .unwrap_or(rect),
        })
    }
}

/// Index of the most specific monitor containing `point`.
///
/// Overlapping monitors are normal: a mirroring arrangement, or an automatic
/// monitor covering the whole screen next to the real ones. Answering with the
/// smallest containing rectangle keeps `active` specific instead of degrading
/// silently to a screen-wide fallback.
fn monitor_index_at(monitors: &[Monitor], point: (i32, i32)) -> Option<usize> {
    let probe = Rect::new(point.0, point.1, 1, 1);
    monitors
        .iter()
        .enumerate()
        .filter(|(_, monitor)| monitor.rect.contains(&probe))
        .min_by_key(|(_, monitor)| i64::from(monitor.rect.w) * i64::from(monitor.rect.h))
        .map(|(index, _)| index)
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
}

impl WindowBackend for X11Backend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::x11()
    }

    fn behavior(&self) -> Behavior {
        Behavior::x11()
    }

    fn list_windows(&self) -> BackendResult<Vec<WindowInfo>> {
        let clients = self.prop_u32(self.root, self.atoms.client_list)?;
        let active = self
            .prop_u32(self.root, self.atoms.active_window)?
            .first()
            .copied();
        let mut windows = Vec::new();
        for win in clients {
            if !self.is_controllable(win)? {
                continue;
            }
            if let Ok(info) = self.window_info(win, active == Some(win)) {
                windows.push(info);
            }
        }
        Ok(windows)
    }

    fn active_window(&self) -> BackendResult<WindowId> {
        if let Some(win) = self
            .prop_u32(self.root, self.atoms.active_window)?
            .first()
            .copied()
        {
            if win != 0 && self.ensure_window(win).is_ok() {
                return Ok(WindowId(win.to_string()));
            }
        }
        // No focus recorded: fall back to the top of the stacking order.
        let stacking = self.prop_u32(self.root, self.atoms.client_list_stacking)?;
        for win in stacking.into_iter().rev() {
            if self.is_controllable(win)? {
                return Ok(WindowId(win.to_string()));
            }
        }
        Err(BackendError::WindowNotFound(
            "no focused window".to_string(),
        ))
    }

    fn geometry(&self, id: &WindowId) -> BackendResult<Geometry> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        let geometry = self
            .conn
            .get_geometry(win)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        let translated = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .reply()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;
        let extents = self.frame_extents(win)?;
        let client = Rect::new(
            translated.dst_x as i32,
            translated.dst_y as i32,
            geometry.width as i32,
            geometry.height as i32,
        );
        Ok(Geometry {
            frame: extents.client_to_frame(client),
            client,
            // X11 coordinates are already the coordinates applications see.
            scale: 1.0,
            decorated: extents != Extents::ZERO,
        })
    }

    fn monitors(&self) -> BackendResult<Vec<Monitor>> {
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(|err| BackendError::Protocol(err.to_string()))
            .and_then(|cookie| {
                cookie
                    .reply()
                    .map_err(|err| BackendError::Protocol(err.to_string()))
            });
        match reply {
            Ok(reply) if !reply.monitors.is_empty() => {
                let mut monitors = Vec::new();
                for monitor in reply.monitors {
                    monitors.push(Monitor {
                        name: self.atom_name(monitor.name)?,
                        rect: Rect::new(
                            monitor.x as i32,
                            monitor.y as i32,
                            monitor.width as i32,
                            monitor.height as i32,
                        ),
                        primary: monitor.primary,
                    });
                }
                Ok(monitors)
            }
            // Servers without RandR 1.5 fall back to the single screen rectangle.
            _ => Ok(vec![self.screen_monitor()?]),
        }
    }

    fn area(&self, monitor: MonitorSelector, area: AreaKind) -> BackendResult<Rect> {
        let monitors = self.monitors()?;
        let index = match monitor {
            MonitorSelector::Index(index) => {
                if index >= monitors.len() {
                    return Err(BackendError::Protocol(format!(
                        "monitor index {index} out of range (found {})",
                        monitors.len()
                    )));
                }
                index
            }
            MonitorSelector::Primary => monitors.iter().position(|m| m.primary).unwrap_or(0),
            MonitorSelector::Active => {
                // "active" is the monitor under the pointer, matching the GNOME
                // extension and the cursor-anchored menu. Deriving it from the
                // focused window instead made the same option pick a different
                // monitor on the two platforms.
                let point = self.pointer_position()?;
                monitor_index_at(&monitors, point)
                    .or_else(|| monitors.iter().position(|m| m.primary))
                    .unwrap_or(0)
            }
        };
        let rect = monitors[index].rect;
        Ok(match area {
            AreaKind::Monitor => rect,
            AreaKind::WorkArea => self
                .work_area_rect()?
                .and_then(|work_area| intersect(rect, work_area))
                .unwrap_or(rect),
        })
    }

    fn apply(
        &self,
        id: &WindowId,
        target: Rect,
        basis: SizeBasis,
        area: Rect,
        allow_outside: bool,
    ) -> BackendResult<GeometryResult> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        // Keep both platforms on the same policy: without the switch the request
        // is pulled into the area here, instead of leaving it to the WM.
        let requested = if allow_outside {
            target
        } else {
            target.clamp_into(area)
        };
        let clamped_by_area = requested != target;
        let outside = !area.contains(&requested);
        let target = requested;

        let mut unmaximized = false;
        if self.maximized(win)? {
            self.set_wm_state(
                win,
                0,
                self.atoms.state_maximized_vert,
                self.atoms.state_maximized_horz,
            )?;
            if !self.wait_until(STATE_TIMEOUT, || !self.maximized(win).unwrap_or(true)) {
                return Err(BackendError::Protocol(
                    "window manager did not leave the maximized state".to_string(),
                ));
            }
            unmaximized = true;
        }

        let baseline = self.settle(win, Duration::from_millis(400))?;
        let client_target = match basis {
            SizeBasis::Client => target,
            SizeBasis::Frame => baseline.extents().frame_to_client(target),
        };
        let hints = self.size_hint_limits(win)?;
        self.send_moveresize(win, client_target)?;
        let (achieved, changed) = self.wait_for_geometry(win, baseline.client, SETTLE_TIMEOUT)?;

        let achieved_rect = match basis {
            SizeBasis::Client => achieved.client,
            SizeBasis::Frame => achieved.frame,
        };
        let resized = achieved_rect.w != target.w || achieved_rect.h != target.h;
        let clamped = clamped_by_area || (resized && (target.w > area.w || target.h > area.h));
        let exact = achieved_rect == target;
        // Only blame the client's hints when the resulting client size really is
        // one of them; every other difference is a request the WM did not honour.
        let hint_limited = hints.is_some_and(|(min, max)| {
            let (w, h) = (achieved.client.w, achieved.client.h);
            let hits =
                |limit: (i32, i32)| (limit.0 > 0 && limit.0 == w) || (limit.1 > 0 && limit.1 == h);
            hits(min) || hits(max)
        });
        let adjusted_by_hints = resized && !clamped && hint_limited;

        Ok(GeometryResult {
            requested: target,
            basis,
            achieved: achieved_rect,
            achieved_frame: achieved.frame,
            achieved_client: achieved.client,
            scale: achieved.scale,
            clamped,
            outside,
            exact,
            adjusted_by_hints,
            // The window did change, just not to the requested size, and neither
            // the area nor the hints account for it.
            ignored: changed && resized && !clamped && !hint_limited,
            unchanged: !changed,
            unmaximized,
        })
    }

    fn set_above(&self, id: &WindowId, above: bool) -> BackendResult<()> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        // Mutter can re-apply the state after the current configure cycle, so a
        // successful read back is not enough; confirm that it sticks and retry once.
        let mut settled = None;
        for attempt in 0..2 {
            self.set_wm_state(win, u32::from(above), self.atoms.state_above, 0)?;
            if !self.wait_until(STATE_TIMEOUT, || self.is_above(win).ok() == Some(above)) {
                settled = None;
                continue;
            }
            sleep(Duration::from_millis(150));
            let state = self.is_above(win)?;
            if state == above {
                return Ok(());
            }
            settled = Some((attempt, state));
        }
        Err(BackendError::Protocol(match settled {
            Some((_, state)) => format!(
                "above state settled at {state}, requested {above}; another component keeps changing it"
            ),
            None => format!(
                "window manager did not turn the above state {}",
                if above { "on" } else { "off" }
            ),
        }))
    }

    fn set_decorated(&self, id: &WindowId, _decorated: bool) -> BackendResult<()> {
        let win = Self::parse_window(id)?;
        self.ensure_window(win)?;
        // Measured under mutter: _MOTIF_WM_HINTS is ignored once a window is
        // mapped, so writing it would report success while changing nothing.
        Err(BackendError::Unsupported(
            "runtime decoration toggling is ignored by mutter/gnome-shell",
        ))
    }
}

impl HotkeyBackend for X11Backend {
    fn register(&self, spec: &HotkeySpec) -> BackendResult<HotkeyReport> {
        let accelerator = keys::parse_accelerator(&spec.accelerator)?;
        let keycode = keys::keycode_for(&self.conn, accelerator.keysym).ok_or_else(|| {
            BackendError::Protocol(format!(
                "keysym {:#x} is not in the keyboard map",
                accelerator.keysym
            ))
        })?;
        match keys::grab(&self.conn, self.root, accelerator.mask, keycode) {
            Ok(()) => {}
            Err(GrabFailure::Conflict(detail)) => {
                return Ok(HotkeyReport {
                    accelerator: spec.accelerator.clone(),
                    outcome: HotkeyOutcome::Conflict { detail },
                });
            }
            Err(GrabFailure::Other(detail)) => return Err(BackendError::Protocol(detail)),
        }
        self.conn
            .flush()
            .map_err(|err| BackendError::Protocol(err.to_string()))?;

        let outcome = match self.probe {
            ProbePolicy::Off => HotkeyOutcome::RegisteredUnverified {
                detail: "delivery probe is off; injecting keys into a live session is opt-in"
                    .to_string(),
            },
            ProbePolicy::Inject => {
                sleep(Duration::from_millis(150));
                let delivered = keys::inject_and_wait(
                    &self.conn,
                    self.root,
                    accelerator,
                    keycode,
                    Duration::from_millis(900),
                )
                .unwrap_or(false);
                if delivered {
                    HotkeyOutcome::Verified
                } else {
                    HotkeyOutcome::RegisteredUnverified {
                        detail: format!(
                            "grab succeeded but the injected probe keystroke never arrived; another component likely owns {}",
                            spec.accelerator
                        ),
                    }
                }
            }
        };
        Ok(HotkeyReport {
            accelerator: spec.accelerator.clone(),
            outcome,
        })
    }

    fn unregister(&self, accelerator: &str) -> BackendResult<()> {
        let parsed = keys::parse_accelerator(accelerator)?;
        let keycode = keys::keycode_for(&self.conn, parsed.keysym).ok_or_else(|| {
            BackendError::Protocol("keysym is not in the keyboard map".to_string())
        })?;
        keys::ungrab(&self.conn, self.root, keycode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_buttons_are_not_hotkey_modifiers() {
        let combo = keys::MOD_MOD4 | keys::MOD_MOD1;
        // Button1Mask..Button5Mask, one bit each.
        for button in [0x0100u16, 0x0200, 0x0400, 0x0800, 0x1000] {
            assert_eq!(hotkey_modifiers(combo | button), combo);
        }
        assert_eq!(hotkey_modifiers(combo | 0x1f00), combo);
    }

    #[test]
    fn lock_and_button_bits_normalize_together() {
        let combo = keys::MOD_CONTROL | keys::MOD_SHIFT;
        assert_eq!(
            hotkey_modifiers(combo | keys::MOD_LOCK | keys::MOD_MOD2 | 0x0100),
            combo
        );
        // The modifier combination itself still has to match exactly.
        assert_ne!(
            hotkey_modifiers(keys::MOD_CONTROL | 0x0100),
            hotkey_modifiers(keys::MOD_CONTROL | keys::MOD_SHIFT | 0x0100)
        );
    }

    fn monitor(name: &str, x: i32, y: i32, w: i32, h: i32, primary: bool) -> Monitor {
        Monitor {
            name: name.to_string(),
            rect: Rect::new(x, y, w, h),
            primary,
        }
    }

    #[test]
    fn active_monitor_prefers_the_smallest_containing_rect() {
        // A screen-wide automatic monitor next to the two real halves, which is
        // what a mutter session reports once xrandr monitors are defined.
        let monitors = vec![
            monitor("screen", 0, 0, 1600, 900, true),
            monitor("sizer-left", 0, 0, 800, 900, false),
            monitor("sizer-right", 800, 0, 800, 900, false),
        ];
        assert_eq!(monitor_index_at(&monitors, (200, 400)), Some(1));
        assert_eq!(monitor_index_at(&monitors, (1200, 400)), Some(2));
        // The last pixel of the left monitor belongs to it, not to the screen-wide one.
        assert_eq!(monitor_index_at(&monitors, (799, 899)), Some(1));
        assert_eq!(monitor_index_at(&monitors, (5000, 5000)), None);
    }

    #[test]
    fn active_monitor_handles_a_single_monitor() {
        let monitors = vec![monitor("screen", 0, 0, 1920, 1080, true)];
        assert_eq!(monitor_index_at(&monitors, (0, 0)), Some(0));
        assert_eq!(monitor_index_at(&monitors, (1919, 1079)), Some(0));
        assert_eq!(monitor_index_at(&monitors, (1920, 1080)), None);
    }
}
