//! `sizer macro`: run a Sizer macro on either platform.
//!
//! The macro language, its selector and its evaluator live in `sizer-macro`.
//! This module only adapts the two window hosts: EWMH through `sizer-backend-x11`
//! and the GNOME shell extension through D-Bus. Both adapters read geometry back
//! after every write, so a result never means "the request was sent".

use std::error::Error;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sizer_backend_x11::X11Backend;
use sizer_core::{Rect, SizeBasis, WindowId};
use sizer_macro::{
    ExecError, MonitorSnapshot, WindowGeometry, WindowHost, WindowSnapshot, execute, parse,
};
use sizer_platform::WindowBackend;

const BUS_NAME: &str = "org.sizerlinux.Shell";
const OBJECT_PATH: &str = "/org/sizerlinux/Shell";
const INTERFACE: &str = "org.sizerlinux.Shell";
/// How long a write may take to settle before the read-back is reported as is.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(2500);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    X11,
    Gnome,
}

impl Backend {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_ascii_lowercase().as_str() {
            "auto" => Ok(Backend::Auto),
            "x11" => Ok(Backend::X11),
            "gnome" | "wayland" => Ok(Backend::Gnome),
            other => Err(format!(
                "unknown backend {other:?}, expected auto, x11 or gnome"
            )),
        }
    }
}

pub fn run(json_output: bool, source: &str, backend: Backend) -> Result<(), Box<dyn Error>> {
    let mac = parse(source)?;
    let (name, host): (&str, Box<dyn WindowHost>) = match backend {
        Backend::X11 => ("x11", Box::new(X11Host::connect()?)),
        Backend::Gnome => ("gnome", Box::new(GnomeHost::connect().map_err(box_error)?)),
        Backend::Auto => match GnomeHost::connect() {
            // The extension lists XWayland windows too, so when it answers it is
            // the better host even in an X11 session.
            Ok(host) => ("gnome", Box::new(host) as Box<dyn WindowHost>),
            Err(_) => ("x11", Box::new(X11Host::connect()?) as Box<dyn WindowHost>),
        },
    };
    let report = execute(&mac, host.as_ref()).map_err(box_error)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": report.failed() == 0,
                "backend": name,
                "windows": report.windows,
                "skipped": report.skipped,
            }))?
        );
    } else {
        println!(
            "{}: {} window(s) processed{}",
            name,
            report.windows.len(),
            if report.skipped.is_empty() {
                String::new()
            } else {
                format!(", {} skipped", report.skipped.len())
            }
        );
        for outcome in &report.windows {
            let geometry = match outcome.requested {
                Some(requested) => format!(
                    "requested {} {}x{} at {},{}",
                    basis_name(outcome.basis),
                    requested.w,
                    requested.h,
                    requested.x,
                    requested.y
                ),
                None => "state only".to_string(),
            };
            println!(
                "#{} {:<6} {:<20} {}x{} at {},{}  {}  {}",
                outcome.index,
                outcome.id,
                truncate(&outcome.title, 20),
                outcome.client.w,
                outcome.client.h,
                outcome.client.x,
                outcome.client.y,
                geometry,
                match (&outcome.error, outcome.exact) {
                    (Some(error), _) => format!("error: {error}"),
                    (None, true) => "exact".to_string(),
                    (None, false) => "not exact".to_string(),
                }
            );
        }
        for skip in &report.skipped {
            println!("skipped: {}", skip.reason);
        }
    }
    if report.failed() > 0 {
        return Err(format!(
            "{} window(s) did not reach the requested geometry",
            report.failed()
        )
        .into());
    }
    Ok(())
}

fn box_error(error: ExecError) -> Box<dyn Error> {
    Box::new(error)
}

fn basis_name(basis: SizeBasis) -> &'static str {
    match basis {
        SizeBasis::Client => "client",
        SizeBasis::Frame => "frame",
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn monitor_of(monitors: &[MonitorSnapshot], rect: Rect) -> usize {
    let probe = Rect::new(rect.center_x(), rect.center_y(), 1, 1);
    monitors
        .iter()
        .enumerate()
        .filter(|(_, monitor)| monitor.rect.contains(&probe))
        .min_by_key(|(_, monitor)| i64::from(monitor.rect.w) * i64::from(monitor.rect.h))
        .map(|(index, _)| index)
        .unwrap_or_else(|| monitors.iter().position(|m| m.primary).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// X11

struct X11Host {
    backend: X11Backend,
}

impl X11Host {
    fn connect() -> Result<Self, sizer_platform::BackendError> {
        Ok(Self {
            backend: X11Backend::connect()?,
        })
    }

    fn snapshot(
        &self,
        info: &sizer_core::WindowInfo,
        monitors: &[MonitorSnapshot],
    ) -> WindowSnapshot {
        let id = info.id.clone();
        WindowSnapshot {
            id: id.0.clone(),
            title: info.title.clone(),
            class: info.app_id.clone(),
            instance: None,
            pid: info.pid,
            desktop: self.backend.window_desktop(&id).unwrap_or(0),
            client: info.geometry.client,
            frame: info.geometry.frame,
            maximized: info.maximized,
            minimized: self.backend.is_minimized(&id).unwrap_or(false),
            monitor: monitor_of(monitors, info.geometry.frame),
            focused: info.focused,
        }
    }
}

impl WindowHost for X11Host {
    fn windows(&self) -> Result<Vec<WindowSnapshot>, ExecError> {
        let monitors = self.monitors()?;
        Ok(self
            .backend
            .list_windows()
            .map_err(exec)?
            .iter()
            .map(|info| self.snapshot(info, &monitors))
            .collect())
    }

    fn active_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        let id = match self.backend.active_window() {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        Ok(self.windows()?.into_iter().find(|window| window.id == id.0))
    }

    fn last_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        let active = self.backend.active_window().ok();
        let windows = self.windows()?;
        let order = self.backend.stacking_order().map_err(exec)?;
        Ok(order
            .iter()
            .filter(|id| active.as_ref() != Some(id))
            .find_map(|id| windows.iter().find(|window| window.id == id.0).cloned()))
    }

    fn monitors(&self) -> Result<Vec<MonitorSnapshot>, ExecError> {
        let monitors = self.backend.monitors().map_err(exec)?;
        let mut snapshots = Vec::with_capacity(monitors.len());
        for (index, monitor) in monitors.iter().enumerate() {
            let work_area = self
                .backend
                .area(
                    sizer_core::MonitorSelector::Index(index),
                    sizer_core::AreaKind::WorkArea,
                )
                .unwrap_or(monitor.rect);
            snapshots.push(MonitorSnapshot {
                rect: monitor.rect,
                work_area,
                primary: monitor.primary,
            });
        }
        Ok(snapshots)
    }

    fn current_monitor(&self) -> Result<usize, ExecError> {
        let monitors = self.monitors()?;
        let (x, y) = self.backend.pointer().map_err(exec)?;
        Ok(monitor_of(&monitors, Rect::new(x, y, 1, 1)))
    }

    fn current_desktop(&self) -> Result<u32, ExecError> {
        self.backend.current_desktop().map_err(exec)
    }

    fn geometry(&self, id: &str) -> Result<WindowGeometry, ExecError> {
        let geometry = self
            .backend
            .geometry(&WindowId(id.to_string()))
            .map_err(exec)?;
        Ok(WindowGeometry {
            client: geometry.client,
            frame: geometry.frame,
        })
    }

    fn apply(&self, id: &str, target: Rect, basis: SizeBasis) -> Result<WindowGeometry, ExecError> {
        // The area only feeds the clamped/hints flags, which the macro report
        // does not use; a screen-sized area keeps a legitimate multi-monitor
        // placement from being labelled clamped.
        let area = Rect::new(0, 0, i32::MAX, i32::MAX);
        let result = self
            .backend
            .apply(&WindowId(id.to_string()), target, basis, area, true)
            .map_err(exec)?;
        Ok(WindowGeometry {
            client: result.achieved_client,
            frame: result.achieved_frame,
        })
    }

    fn set_maximized(&self, id: &str, maximized: bool) -> Result<(), ExecError> {
        let achieved = self
            .backend
            .set_maximized(&WindowId(id.to_string()), maximized)
            .map_err(exec)?;
        if achieved != maximized {
            return Err(ExecError::new(format!(
                "window {id} did not turn maximization {}",
                if maximized { "on" } else { "off" }
            )));
        }
        Ok(())
    }

    fn set_minimized(&self, id: &str, minimized: bool) -> Result<(), ExecError> {
        let id = WindowId(id.to_string());
        let achieved = self.backend.set_minimized(&id, minimized).map_err(exec)?;
        if achieved != minimized {
            return Err(ExecError::new(format!(
                "window {} did not turn the minimized state {}",
                id.0,
                if minimized { "on" } else { "off" }
            )));
        }
        Ok(())
    }
}

fn exec(error: sizer_platform::BackendError) -> ExecError {
    ExecError::new(error.to_string())
}

// ---------------------------------------------------------------------------
// GNOME shell extension

struct GnomeHost {
    connection: zbus::blocking::Connection,
}

impl GnomeHost {
    fn connect() -> Result<Self, ExecError> {
        let connection = zbus::blocking::Connection::session().map_err(|error| {
            ExecError::new(format!("cannot connect to the session bus: {error}"))
        })?;
        let host = Self { connection };
        let pong: String = host.call("Ping", &()).map_err(|_| {
            ExecError::new("the GNOME shell extension does not answer on org.sizerlinux.Shell")
        })?;
        if pong != "pong" {
            return Err(ExecError::new(format!(
                "the GNOME shell extension answered {pong:?} instead of pong"
            )));
        }
        Ok(host)
    }

    fn proxy(&self) -> Result<zbus::blocking::Proxy<'_>, ExecError> {
        zbus::blocking::Proxy::new(&self.connection, BUS_NAME, OBJECT_PATH, INTERFACE)
            .map_err(|error| ExecError::new(format!("shell extension proxy: {error}")))
    }

    fn call<T>(&self, method: &str, args: &T) -> Result<String, ExecError>
    where
        T: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.proxy()?
            .call(method, args)
            .map_err(|error| ExecError::new(format!("{method}: {error}")))
    }

    fn shell_info(&self) -> Result<Value, ExecError> {
        let reply = self.call("ShellInfo", &())?;
        serde_json::from_str(&reply)
            .map_err(|error| ExecError::new(format!("ShellInfo returned invalid JSON: {error}")))
    }

    fn window_info(&self, id: &str) -> Result<WindowSnapshot, ExecError> {
        let reply = self.call("GetWindow", &(id,))?;
        let value: Value = serde_json::from_str(&reply)
            .map_err(|error| ExecError::new(format!("GetWindow returned invalid JSON: {error}")))?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ExecError::new(format!(
                "window {id} is gone: {}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        snapshot_from(&value["window"])
            .ok_or_else(|| ExecError::new(format!("GetWindow returned no geometry for {id}")))
    }

    /// Wait until the compositor has produced the requested geometry.
    ///
    /// A window that is still mapping reports a degenerate frame; that is never
    /// a legitimate answer to a positive request, so it does not count as
    /// settled. A real adjustment (size hints) is accepted after a short
    /// plateau and reported as not exact.
    fn settle(
        &self,
        id: &str,
        wanted: Rect,
        basis: SizeBasis,
    ) -> Result<WindowGeometry, ExecError> {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        let mut previous = self.window_info(id)?;
        let mut stable = 0;
        loop {
            let current = previous;
            let degenerate = current.client.w <= 0 || current.client.h <= 0;
            if current.rect(basis) == wanted
                || (stable >= 6 && !degenerate)
                || Instant::now() >= deadline
            {
                return Ok(WindowGeometry {
                    client: current.client,
                    frame: current.frame,
                });
            }
            std::thread::sleep(POLL_INTERVAL);
            previous = self.window_info(id)?;
            if previous.client == current.client && previous.frame == current.frame {
                stable += 1;
            } else {
                stable = 0;
            }
        }
    }
}

impl WindowHost for GnomeHost {
    fn windows(&self) -> Result<Vec<WindowSnapshot>, ExecError> {
        let reply = self.call("ListWindows", &())?;
        let values: Vec<Value> = serde_json::from_str(&reply).map_err(|error| {
            ExecError::new(format!("ListWindows returned invalid JSON: {error}"))
        })?;
        let mut windows: Vec<(i64, WindowSnapshot)> = values
            .iter()
            .map(|value| {
                (
                    value
                        .get("stable_sequence")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    value,
                )
            })
            .filter_map(|(sequence, value)| {
                snapshot_from(value).map(|snapshot| (sequence, snapshot))
            })
            .collect();
        windows.sort_by_key(|(sequence, _)| *sequence);
        Ok(windows.into_iter().map(|(_, window)| window).collect())
    }

    fn active_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        Ok(self.windows()?.into_iter().find(|window| window.focused))
    }

    fn last_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        let reply = self.call("LastWindow", &())?;
        let value: Value = serde_json::from_str(&reply).map_err(|error| {
            ExecError::new(format!("LastWindow returned invalid JSON: {error}"))
        })?;
        Ok(value.get("window").and_then(snapshot_from))
    }

    fn monitors(&self) -> Result<Vec<MonitorSnapshot>, ExecError> {
        let info = self.shell_info()?;
        let mut monitors = Vec::new();
        for value in info["monitors"].as_array().into_iter().flatten() {
            let Some(rect) = rect_from(&value["geometry"]) else {
                continue;
            };
            monitors.push(MonitorSnapshot {
                rect,
                work_area: rect_from(&value["work_area"]).unwrap_or(rect),
                primary: value
                    .get("primary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        Ok(monitors)
    }

    fn current_monitor(&self) -> Result<usize, ExecError> {
        let info = self.shell_info()?;
        Ok(info
            .get("current_monitor")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
    }

    fn current_desktop(&self) -> Result<u32, ExecError> {
        let info = self.shell_info()?;
        Ok(info
            .get("current_workspace")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32)
    }

    fn geometry(&self, id: &str) -> Result<WindowGeometry, ExecError> {
        let window = self.window_info(id)?;
        Ok(WindowGeometry {
            client: window.client,
            frame: window.frame,
        })
    }

    fn apply(&self, id: &str, target: Rect, basis: SizeBasis) -> Result<WindowGeometry, ExecError> {
        let reply = self.call(
            "MoveResize",
            &(
                id,
                target.x,
                target.y,
                target.w,
                target.h,
                basis_name(basis),
            ),
        )?;
        let value: Value = serde_json::from_str(&reply).map_err(|error| {
            ExecError::new(format!("MoveResize returned invalid JSON: {error}"))
        })?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ExecError::new(format!(
                "MoveResize for {id} failed: {}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        self.settle(id, target, basis)
    }

    fn set_maximized(&self, id: &str, maximized: bool) -> Result<(), ExecError> {
        let reply = self.call("SetMaximized", &(id, maximized))?;
        let value: Value = serde_json::from_str(&reply).map_err(|error| {
            ExecError::new(format!("SetMaximized returned invalid JSON: {error}"))
        })?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ExecError::new(format!(
                "SetMaximized for {id} failed: {}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        self.wait_state(id, "maximized", maximized)
    }

    fn set_minimized(&self, id: &str, minimized: bool) -> Result<(), ExecError> {
        let reply = self.call("SetMinimized", &(id, minimized))?;
        let value: Value = serde_json::from_str(&reply).map_err(|error| {
            ExecError::new(format!("SetMinimized returned invalid JSON: {error}"))
        })?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ExecError::new(format!(
                "SetMinimized for {id} failed: {}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        self.wait_state(id, "minimized", minimized)
    }
}

impl GnomeHost {
    fn wait_state(&self, id: &str, key: &str, wanted: bool) -> Result<(), ExecError> {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        loop {
            let reply = self.call("GetWindow", &(id,))?;
            let value: Value = serde_json::from_str(&reply).map_err(|error| {
                ExecError::new(format!("GetWindow returned invalid JSON: {error}"))
            })?;
            let state = value["window"].get(key).and_then(|state| {
                state
                    .as_bool()
                    .or_else(|| state.as_i64().map(|value| value != 0))
            });
            if state == Some(wanted) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ExecError::new(format!(
                    "window {id} did not turn {key} {}",
                    if wanted { "on" } else { "off" }
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

fn rect_from(value: &Value) -> Option<Rect> {
    Some(Rect::new(
        value.get("x")?.as_i64()? as i32,
        value.get("y")?.as_i64()? as i32,
        value.get("w")?.as_i64()? as i32,
        value.get("h")?.as_i64()? as i32,
    ))
}

fn snapshot_from(value: &Value) -> Option<WindowSnapshot> {
    Some(WindowSnapshot {
        id: value.get("id")?.as_str()?.to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        class: value
            .get("wm_class")
            .and_then(Value::as_str)
            .map(str::to_string),
        instance: value
            .get("wm_class_instance")
            .and_then(Value::as_str)
            .map(str::to_string),
        pid: value
            .get("pid")
            .and_then(Value::as_i64)
            .map(|pid| pid as u32),
        desktop: if value
            .get("on_all_workspaces")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            u32::MAX
        } else {
            value
                .get("workspace")
                .and_then(Value::as_i64)
                .filter(|workspace| *workspace >= 0)
                .unwrap_or(0) as u32
        },
        client: rect_from(&value["client"])?,
        frame: rect_from(&value["frame"])?,
        maximized: value
            .get("maximized")
            .and_then(|state| state.as_i64().or_else(|| state.as_bool().map(i64::from)))
            .is_some_and(|state| state != 0),
        minimized: value
            .get("minimized")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        monitor: value.get("monitor").and_then(Value::as_u64).unwrap_or(0) as usize,
        focused: value
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}
