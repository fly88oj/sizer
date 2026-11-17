//! Backend traits plus the declarative capability/behaviour layer.
//!
//! Phase 0 found that the same user action needs different mechanics per platform,
//! and that some of them fail silently (raw `ConfigureWindow` under mutter, key
//! grabs whose events never arrive). Those findings are encoded here as data:
//! every backend declares [`Capabilities`] (what users can do) and [`Behavior`]
//! (how the backend must talk to the platform). Call sites branch on these
//! values instead of scattering platform `if`s through the code.

use serde::Serialize;
use sizer_core::{
    AreaKind, Geometry, GeometryResult, MonitorSelector, Rect, SizeBasis, WindowId, WindowInfo,
};

/// What a backend can actually do. Drives CLI/UI visibility and `doctor` output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Capabilities {
    pub backend: &'static str,
    /// Window geometry can be read and written.
    pub geometry: bool,
    /// Windows can be kept above others.
    pub stay_above: bool,
    /// Server-side decorations can be toggled.
    pub decorations: bool,
    /// A global hotkey can be registered.
    pub hotkeys: bool,
    /// A conflicting hotkey produces a diagnosable error.
    pub hotkey_conflict_detectable: bool,
    /// Registration alone is not enough; delivery must be probed (Phase 0 finding).
    pub hotkey_delivery_probe: bool,
}

impl Capabilities {
    pub fn x11() -> Self {
        Self {
            backend: "x11",
            geometry: true,
            stay_above: true,
            // Measured under mutter: _MOTIF_WM_HINTS is ignored for mapped
            // windows, so runtime decoration toggling cannot be promised.
            decorations: false,
            hotkeys: true,
            hotkey_conflict_detectable: true,
            hotkey_delivery_probe: true,
        }
    }
}

/// How a backend must perform a geometry write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GeometryWrite {
    /// X11: always `_NET_MOVERESIZE_WINDOW` with StaticGravity. Raw
    /// `ConfigureWindow` is ignored by mutter once the window is managed.
    EwmhMoveresize,
    /// GNOME Shell: `Meta.Window.move_resize_frame` over D-Bus.
    FrameMoveResize,
}

/// Whether decorations can be changed at all, and through which mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DecorationControl {
    None,
    ServerSide,
}

/// Hotkey mechanics that differ per platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HotkeyPolicy {
    /// The grab can be refused, and the refusal is reported (X11 BadAccess).
    pub conflict_detectable: bool,
    /// Registration can succeed while events never arrive, so a delivery probe
    /// has to run before telling the user the hotkey works.
    pub requires_delivery_probe: bool,
}

/// Declarative platform behaviour, one instance per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Behavior {
    pub geometry_write: GeometryWrite,
    /// A maximized window ignores geometry requests until it is restored.
    pub maximized_blocks_geometry: bool,
    /// A resize may be refused by client size hints; the result must say so.
    pub hints_can_adjust: bool,
    pub decorations: DecorationControl,
    pub hotkeys: HotkeyPolicy,
}

impl Behavior {
    pub fn x11() -> Self {
        Self {
            geometry_write: GeometryWrite::EwmhMoveresize,
            maximized_blocks_geometry: true,
            hints_can_adjust: true,
            // `set_decorated` returns Unsupported: mutter ignores
            // `_MOTIF_WM_HINTS` on managed windows, so the capability that
            // matters (`Capabilities::decorations`) is false as well.
            decorations: DecorationControl::None,
            hotkeys: HotkeyPolicy {
                conflict_detectable: true,
                requires_delivery_probe: true,
            },
        }
    }
}

/// Which window an action targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSelector {
    Active,
    Id(WindowId),
    TitleContains(String),
    AppId(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BackendError {
    Unavailable(String),
    WindowNotFound(String),
    Unsupported(&'static str),
    Protocol(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unavailable(msg) => write!(f, "backend unavailable: {msg}"),
            BackendError::WindowNotFound(msg) => write!(f, "window not found: {msg}"),
            BackendError::Unsupported(feature) => {
                write!(f, "unsupported by this backend: {feature}")
            }
            BackendError::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for BackendError {}

pub type BackendResult<T> = Result<T, BackendError>;

/// One output as the backend sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Monitor {
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}

/// Window geometry and stacking control.
pub trait WindowBackend {
    fn capabilities(&self) -> Capabilities;
    fn behavior(&self) -> Behavior;

    fn list_windows(&self) -> BackendResult<Vec<WindowInfo>>;
    fn active_window(&self) -> BackendResult<WindowId>;
    fn geometry(&self, id: &WindowId) -> BackendResult<Geometry>;

    /// Outputs in the backend's logical coordinate space, primary flag included.
    fn monitors(&self) -> BackendResult<Vec<Monitor>>;

    /// Reference area for `area`/`monitor`, in logical pixels.
    fn area(&self, monitor: MonitorSelector, area: AreaKind) -> BackendResult<Rect>;

    /// Apply a target rectangle expressed in `basis` coordinates.
    ///
    /// `area` is the reference area the caller resolved for this request;
    /// `clamped` is reported against it so a multi-monitor placement is not
    /// compared with the monitor the window happened to land on. Unless
    /// `allow_outside` is set, the position is pulled into `area` here, so every
    /// backend follows the same policy instead of the window manager's.
    fn apply(
        &self,
        id: &WindowId,
        target: Rect,
        basis: SizeBasis,
        area: Rect,
        allow_outside: bool,
    ) -> BackendResult<GeometryResult>;

    fn set_above(&self, id: &WindowId, above: bool) -> BackendResult<()>;

    fn set_decorated(&self, id: &WindowId, decorated: bool) -> BackendResult<()>;
}

/// Turn a selector into a concrete window through the backend's own listings.
pub fn resolve_selector(
    backend: &dyn WindowBackend,
    selector: &WindowSelector,
) -> BackendResult<WindowId> {
    match selector {
        WindowSelector::Active => backend.active_window(),
        WindowSelector::Id(id) => Ok(id.clone()),
        WindowSelector::TitleContains(needle) => {
            let needle = needle.to_lowercase();
            backend
                .list_windows()?
                .into_iter()
                .find(|win| win.title.to_lowercase().contains(&needle))
                .map(|win| win.id)
                .ok_or_else(|| BackendError::WindowNotFound(format!("title contains {needle:?}")))
        }
        WindowSelector::AppId(wanted) => backend
            .list_windows()?
            .into_iter()
            .find(|win| {
                win.app_id
                    .as_deref()
                    .is_some_and(|app| app.eq_ignore_ascii_case(wanted))
            })
            .map(|win| win.id)
            .ok_or_else(|| BackendError::WindowNotFound(format!("app id {wanted:?}"))),
    }
}

/// One global hotkey request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeySpec {
    /// e.g. `"<Super><Alt>s"`.
    pub accelerator: String,
}

/// Result of registering a hotkey, including the delivery self-check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HotkeyOutcome {
    /// Registered and a probe keystroke was observed by the owner.
    Verified,
    /// Registered but the probe never arrived, so another component may own it.
    RegisteredUnverified { detail: String },
    /// Registration itself was refused (another client owns the combination).
    Conflict { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HotkeyReport {
    pub accelerator: String,
    pub outcome: HotkeyOutcome,
}

/// Global hotkey registration with the Phase 0 self-check built in.
pub trait HotkeyBackend {
    fn register(&self, spec: &HotkeySpec) -> BackendResult<HotkeyReport>;
    fn unregister(&self, accelerator: &str) -> BackendResult<()>;
}
