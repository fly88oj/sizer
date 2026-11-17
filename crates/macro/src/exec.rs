//! Execution of a parsed macro against a window host.
//!
//! The host is a trait so the X11 backend and the GNOME Shell extension run the
//! same parsed [`Macro`] through the same selector and the same evaluator. Rules
//! that the railroad diagram leaves implicit are decided here, once:
//!
//! - a `resize` block without a window qualifier targets the active window;
//! - a qualifier without a trailing `*` targets the first match only, while
//!   `$_count` still reports how many windows matched;
//! - matches are processed in creation order (X11 `_NET_CLIENT_LIST`, GNOME
//!   `stable_sequence`), so the same macro numbers windows the same way on both
//!   platforms;
//! - `proc[...]` matches the process name (from `/proc/<pid>/comm`) or the WM
//!   class, `cap[...]` matches the window title, both case-insensitively by
//!   substring;
//! - `:desktop` restricts a match to the current desktop/workspace;
//! - every window is read back after the write, so the report carries achieved
//!   geometry instead of the request.

use std::collections::HashMap;

use serde::Serialize;
use sizer_core::{Rect, SizeBasis};

use crate::ast::{
    Action, Axis, DimArea, Expr, Filter, Item, Macro, MacroOption, MonitorRef, ResizeBlock, Span,
    WindowSpec,
};
use crate::eval::{EvalError, Facts, eval};

/// One window as the host sees it, before the macro changes anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowSnapshot {
    pub id: String,
    pub title: String,
    pub class: Option<String>,
    pub instance: Option<String>,
    pub pid: Option<u32>,
    /// Desktop/workspace index; `u32::MAX` for a window on every desktop.
    pub desktop: u32,
    pub client: Rect,
    pub frame: Rect,
    pub maximized: bool,
    pub minimized: bool,
    pub monitor: usize,
    pub focused: bool,
}

impl WindowSnapshot {
    pub fn rect(&self, basis: SizeBasis) -> Rect {
        match basis {
            SizeBasis::Client => self.client,
            SizeBasis::Frame => self.frame,
        }
    }
}

/// One output as the host sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MonitorSnapshot {
    pub rect: Rect,
    pub work_area: Rect,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WindowGeometry {
    pub client: Rect,
    pub frame: Rect,
}

impl WindowGeometry {
    pub fn rect(&self, basis: SizeBasis) -> Rect {
        match basis {
            SizeBasis::Client => self.client,
            SizeBasis::Frame => self.frame,
        }
    }
}

/// What a macro can ask of a platform.
pub trait WindowHost {
    fn windows(&self) -> Result<Vec<WindowSnapshot>, ExecError>;
    fn active_window(&self) -> Result<Option<WindowSnapshot>, ExecError>;
    /// The window used before the active one, for `last`.
    fn last_window(&self) -> Result<Option<WindowSnapshot>, ExecError>;
    fn monitors(&self) -> Result<Vec<MonitorSnapshot>, ExecError>;
    /// Monitor under the pointer, for `current`/`active` monitor qualifiers.
    fn current_monitor(&self) -> Result<usize, ExecError>;
    /// Desktop/workspace shown right now, for `:desktop`.
    fn current_desktop(&self) -> Result<u32, ExecError>;
    fn geometry(&self, id: &str) -> Result<WindowGeometry, ExecError>;
    /// Write `target` in `basis`, wait for the window to settle and read it back.
    fn apply(&self, id: &str, target: Rect, basis: SizeBasis) -> Result<WindowGeometry, ExecError>;
    fn set_maximized(&self, id: &str, maximized: bool) -> Result<(), ExecError>;
    fn set_minimized(&self, id: &str, minimized: bool) -> Result<(), ExecError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError {
    pub message: String,
    pub span: Option<Span>,
}

impl ExecError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }

    fn at(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
        }
    }
}

impl core::fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.span {
            Some(span) => write!(formatter, "{} at {}", self.message, span),
            None => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for ExecError {}

/// What one `resize` block did to one window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub id: String,
    pub title: String,
    pub class: Option<String>,
    /// 1-based position among the windows this block matched (`$_index`).
    pub index: u32,
    /// Number of windows the block matched (`$_count`).
    pub count: u32,
    pub basis: SizeBasis,
    /// Absent when the block only changed state (restore/minimize/maximize).
    pub requested: Option<Rect>,
    pub client: Rect,
    pub frame: Rect,
    pub exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A block that matched nothing, skipped because `ignore_runtime_errors` is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skip {
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    pub windows: Vec<Outcome>,
    pub skipped: Vec<Skip>,
}

impl Report {
    /// True when every window that was asked for geometry reached it.
    pub fn exact(&self) -> bool {
        self.windows
            .iter()
            .all(|outcome| outcome.exact && outcome.error.is_none())
    }

    /// Windows that missed the requested geometry without an error being the
    /// cause. A failure recorded under `ignore_runtime_errors` is not counted:
    /// the macro asked to continue.
    pub fn failed(&self) -> usize {
        self.windows
            .iter()
            .filter(|outcome| outcome.error.is_none() && !outcome.exact)
            .count()
    }
}

/// Run a macro and report what every affected window ended up with.
pub fn execute(mac: &Macro, host: &dyn WindowHost) -> Result<Report, ExecError> {
    let mut executor = Executor {
        host,
        globals: Vec::new(),
        compensate_frame: false,
        ignore_errors: false,
        report: Report::default(),
    };
    for item in &mac.items {
        match item {
            Item::Option {
                option, enabled, ..
            } => match option {
                MacroOption::CompensateFrameEffects => executor.compensate_frame = *enabled,
                MacroOption::IgnoreRuntimeErrors => executor.ignore_errors = *enabled,
            },
            Item::Assign { name, value, .. } => {
                // Stored unevaluated: a variable such as `$gap = w:mon / 10`
                // depends on the window, so it is evaluated per target.
                if let Some(entry) = executor
                    .globals
                    .iter_mut()
                    .find(|(existing, _)| existing == name)
                {
                    entry.1 = value.clone();
                } else {
                    executor.globals.push((name.clone(), value.clone()));
                }
            }
            Item::Resize(block) => executor.block(block)?,
        }
    }
    Ok(executor.report)
}

struct Executor<'a> {
    host: &'a dyn WindowHost,
    globals: Vec<(String, Expr)>,
    compensate_frame: bool,
    ignore_errors: bool,
    report: Report,
}

impl Executor<'_> {
    fn basis(&self) -> SizeBasis {
        if self.compensate_frame {
            SizeBasis::Frame
        } else {
            SizeBasis::Client
        }
    }

    fn block(&mut self, block: &ResizeBlock) -> Result<(), ExecError> {
        let windows = self.host.windows()?;
        let active = self.host.active_window()?;
        let last = self.host.last_window()?;
        let monitors = self.host.monitors()?;
        let current_monitor = self.host.current_monitor()?;
        let current_desktop = self.host.current_desktop()?;
        let processes = process_names(&windows);

        let resolver = Resolver {
            windows: &windows,
            active: active.as_ref(),
            last: last.as_ref(),
            current_desktop,
            processes: &processes,
        };
        let mut matched = resolver.select(&block.filters);
        if block.filters.is_empty() {
            matched = active.iter().collect();
        }
        if matched.is_empty() {
            let reason = describe_filters(&block.filters);
            if self.ignore_errors {
                self.report.skipped.push(Skip {
                    reason: format!("no window matched {reason}; skipped"),
                });
                return Ok(());
            }
            return Err(ExecError::at(
                format!("no window matched {reason}"),
                block.span,
            ));
        }

        let count = matched.len() as u32;
        if !block.all {
            matched.truncate(1);
        }
        let ids: Vec<String> = matched.iter().map(|window| window.id.clone()).collect();
        for (position, id) in ids.iter().enumerate() {
            let window = windows
                .iter()
                .find(|window| &window.id == id)
                .expect("selection only returns listed windows");
            let index = position as u32 + 1;
            let result = self.window(
                block,
                window,
                index,
                count,
                &resolver,
                &monitors,
                current_monitor,
            );
            match result {
                Ok(outcome) => self.report.windows.push(outcome),
                Err(error) if self.ignore_errors => self.report.windows.push(Outcome {
                    id: window.id.clone(),
                    title: window.title.clone(),
                    class: window.class.clone(),
                    index,
                    count,
                    basis: self.basis(),
                    requested: None,
                    client: window.client,
                    frame: window.frame,
                    exact: false,
                    error: Some(error.to_string()),
                }),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn window(
        &self,
        block: &ResizeBlock,
        window: &WindowSnapshot,
        index: u32,
        count: u32,
        resolver: &Resolver<'_>,
        monitors: &[MonitorSnapshot],
        current_monitor: usize,
    ) -> Result<Outcome, ExecError> {
        let facts = FactsCtx {
            resolver,
            current: window,
            monitors,
            current_monitor,
            default_basis: self.basis(),
        };
        let mut scope = self.scope(&facts)?;
        scope.insert("$_index".to_string(), f64::from(index));
        scope.insert("$_count".to_string(), f64::from(count));

        let mut target = window.rect(self.basis());
        let mut requested = false;
        // State changes made by this block so `restore` also undoes them.
        let mut minimized = window.minimized;
        let mut maximized = window.maximized;
        for action in &block.body {
            match action {
                Action::Assign { axis, value, span } => {
                    let value = eval(value, &scope, &facts)
                        .map_err(|error| ExecError::at(error.to_string(), *span))?
                        .round() as i32;
                    match axis {
                        Axis::Top => target.y = value,
                        Axis::Left => target.x = value,
                        Axis::Width => target.w = value.max(1),
                        Axis::Height => target.h = value.max(1),
                        // The parser rejects these; kept exhaustive on purpose.
                        Axis::Right | Axis::Bottom => {
                            return Err(ExecError::at(
                                format!("`{}` cannot be assigned", axis.letter()),
                                *span,
                            ));
                        }
                    }
                    requested = true;
                }
                Action::Variable { name, value, span } => {
                    let value = eval(value, &scope, &facts)
                        .map_err(|error| ExecError::at(error.to_string(), *span))?;
                    scope.insert(name.clone(), value);
                }
                Action::Restore { span } => {
                    // Do not touch state the window is not in: an `_NET_ACTIVE_WINDOW`
                    // for a window that is already visible is an activation, not a
                    // restore, and it makes the follow-up unmaximize racy.
                    if minimized {
                        self.host
                            .set_minimized(&window.id, false)
                            .map_err(|error| ExecError::at(error.message, *span))?;
                        minimized = false;
                    }
                    if maximized {
                        self.host
                            .set_maximized(&window.id, false)
                            .map_err(|error| ExecError::at(error.message, *span))?;
                        maximized = false;
                    }
                }
                Action::Minimize { span } => {
                    self.host
                        .set_minimized(&window.id, true)
                        .map_err(|error| ExecError::at(error.message, *span))?;
                    minimized = true;
                }
                Action::Maximize { monitor, span } => {
                    // A monitor qualifier means "maximize on that monitor": move
                    // the frame there first, because both platforms maximize on
                    // the monitor the window currently occupies.
                    if let Some(reference) = monitor {
                        let index = facts
                            .monitor_index(Some(*reference))
                            .map_err(|error| ExecError::at(error.to_string(), *span))?;
                        if index != window.monitor {
                            self.host
                                .apply(&window.id, monitors[index].rect, SizeBasis::Frame)
                                .map_err(|error| ExecError::at(error.message, *span))?;
                        }
                    }
                    self.host
                        .set_maximized(&window.id, true)
                        .map_err(|error| ExecError::at(error.message, *span))?;
                    maximized = true;
                }
            }
        }

        let geometry = if requested {
            self.host.apply(&window.id, target, self.basis())?
        } else {
            self.host.geometry(&window.id)?
        };
        let achieved = geometry.rect(self.basis());
        Ok(Outcome {
            id: window.id.clone(),
            title: window.title.clone(),
            class: window.class.clone(),
            index,
            count,
            basis: self.basis(),
            requested: requested.then_some(target),
            client: geometry.client,
            frame: geometry.frame,
            exact: !requested || achieved == target,
            error: None,
        })
    }

    fn scope(&self, facts: &FactsCtx<'_>) -> Result<HashMap<String, f64>, ExecError> {
        let mut scope = HashMap::new();
        for (name, expression) in &self.globals {
            let value = eval(expression, &scope, facts)
                .map_err(|error| ExecError::new(format!("variable `{name}`: {error}")))?;
            scope.insert(name.clone(), value);
        }
        Ok(scope)
    }
}

/// Process name per pid, read from `/proc`; missing entries fall back to the
/// window class in [`Resolver::matches`].
fn process_names(windows: &[WindowSnapshot]) -> HashMap<u32, String> {
    let mut names = HashMap::new();
    for window in windows {
        if let Some(pid) = window.pid {
            if let std::collections::hash_map::Entry::Vacant(entry) = names.entry(pid) {
                if let Some(name) = process_name(pid) {
                    entry.insert(name);
                }
            }
        }
    }
    names
}

fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

/// Window-qualifier resolution shared by blocks and dimension suffixes.
struct Resolver<'a> {
    windows: &'a [WindowSnapshot],
    active: Option<&'a WindowSnapshot>,
    last: Option<&'a WindowSnapshot>,
    current_desktop: u32,
    processes: &'a HashMap<u32, String>,
}

impl<'a> Resolver<'a> {
    fn select(&self, filters: &[Filter]) -> Vec<&'a WindowSnapshot> {
        if filters.is_empty() {
            return self.active.into_iter().collect();
        }
        let mut current: Vec<&WindowSnapshot> = self.windows.iter().collect();
        for filter in filters {
            match filter {
                Filter::Last => {
                    current.retain(|window| Some(*window) == self.last);
                }
                Filter::Active => {
                    current.retain(|window| Some(*window) == self.active);
                }
                Filter::Proc {
                    text,
                    desktop,
                    index,
                }
                | Filter::Cap {
                    text,
                    desktop,
                    index,
                } => {
                    let caption = matches!(filter, Filter::Cap { .. });
                    let needle = text.to_lowercase();
                    current.retain(|window| self.matches(window, &needle, caption));
                    if *desktop {
                        current.retain(|window| self.on_current_desktop(window));
                    }
                    if let Some(wanted) = index {
                        let wanted = *wanted as usize;
                        if wanted == 0 || wanted > current.len() {
                            current.clear();
                        } else {
                            let mut kept = vec![current[wanted - 1]];
                            std::mem::swap(&mut current, &mut kept);
                        }
                    }
                }
            }
        }
        current
    }

    fn matches(&self, window: &WindowSnapshot, needle: &str, caption: bool) -> bool {
        if caption {
            return window.title.to_lowercase().contains(needle);
        }
        if let Some(pid) = window.pid {
            if self
                .processes
                .get(&pid)
                .is_some_and(|name| name.to_lowercase().contains(needle))
            {
                return true;
            }
        }
        [window.class.as_deref(), window.instance.as_deref()]
            .into_iter()
            .flatten()
            .any(|name| name.to_lowercase().contains(needle))
    }

    fn on_current_desktop(&self, window: &WindowSnapshot) -> bool {
        window.desktop == u32::MAX || window.desktop == self.current_desktop
    }
}

fn describe_filters(filters: &[Filter]) -> String {
    if filters.is_empty() {
        return "the active window".to_string();
    }
    let parts: Vec<String> = filters
        .iter()
        .map(|filter| match filter {
            Filter::Last => "last".to_string(),
            Filter::Active => "active".to_string(),
            Filter::Proc {
                text,
                desktop,
                index,
            } => describe_filter("proc", text, *desktop, *index),
            Filter::Cap {
                text,
                desktop,
                index,
            } => describe_filter("cap", text, *desktop, *index),
        })
        .collect();
    parts.join(":")
}

fn describe_filter(kind: &str, text: &str, desktop: bool, index: Option<u32>) -> String {
    let mut description = format!("{kind}[{text}]");
    if desktop {
        description.push_str(":desktop");
    }
    if let Some(index) = index {
        description.push_str(&format!(":{index}"));
    }
    description
}

struct FactsCtx<'a> {
    resolver: &'a Resolver<'a>,
    current: &'a WindowSnapshot,
    monitors: &'a [MonitorSnapshot],
    current_monitor: usize,
    default_basis: SizeBasis,
}

impl FactsCtx<'_> {
    fn window_for(&self, spec: &WindowSpec) -> Result<&WindowSnapshot, EvalError> {
        self.resolver
            .select(&spec.filters)
            .into_iter()
            .next()
            .ok_or_else(|| EvalError {
                message: format!("no window matched {}", describe_filters(&spec.filters)),
            })
    }

    fn monitor_index(&self, reference: Option<MonitorRef>) -> Result<usize, EvalError> {
        let count = self.monitors.len();
        if count == 0 {
            return Err(EvalError {
                message: "the backend reports no monitors".to_string(),
            });
        }
        let index = match reference {
            None => self.current.monitor,
            Some(MonitorRef::Primary) => self
                .monitors
                .iter()
                .position(|monitor| monitor.primary)
                .unwrap_or(0),
            Some(MonitorRef::Current) => self.current_monitor,
            Some(MonitorRef::Index(index)) => index as usize,
        };
        if index >= count {
            return Err(EvalError {
                message: format!(
                    "monitor index {index} is out of range (the backend reports {count})"
                ),
            });
        }
        Ok(index)
    }

    fn area_rect(&self, area: &DimArea) -> Result<Rect, EvalError> {
        Ok(match area {
            DimArea::Default => self.current.rect(self.default_basis),
            DimArea::Client(None) => self.current.client,
            DimArea::Win(None) => self.current.frame,
            DimArea::Client(Some(spec)) => self.window_for(spec)?.client,
            DimArea::Win(Some(spec)) => self.window_for(spec)?.frame,
            DimArea::Mon(reference) => self.monitors[self.monitor_index(*reference)?].rect,
            DimArea::WorkArea(reference) => {
                self.monitors[self.monitor_index(*reference)?].work_area
            }
        })
    }
}

impl Facts for FactsCtx<'_> {
    fn dimension(&self, axis: Axis, area: &DimArea) -> Result<f64, EvalError> {
        let rect = self.area_rect(area)?;
        Ok(f64::from(match axis {
            Axis::Top => rect.y,
            Axis::Left => rect.x,
            Axis::Right => rect.right(),
            Axis::Bottom => rect.bottom(),
            Axis::Width => rect.w,
            Axis::Height => rect.h,
        }))
    }
}
