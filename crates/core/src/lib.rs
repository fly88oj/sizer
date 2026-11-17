//! Platform-independent geometry model for Sizer Linux.
//!
//! Semantics that the rest of the product relies on:
//! - Every geometry exchange carries both the frame rectangle (what the window
//!   manager positions) and the client rectangle (what the application draws).
//! - The default user-facing size is the *client* size, matching Sizer's `client`
//!   dimension. Callers that want decorations use [`SizeBasis::Frame`].
//! - All results report what actually happened through [`GeometryResult`] flags
//!   instead of silently succeeding.

use serde::{Deserialize, Serialize};

/// Rectangle in global, scale-independent logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn center_x(&self) -> i32 {
        self.x + self.w / 2
    }

    pub fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }

    pub fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Pull `self` inside `area` without changing its size.
    ///
    /// A rect larger than the area is pinned to the area's origin instead of
    /// being shrunk: sizes are the window manager's business, positions are ours.
    pub fn clamp_into(&self, area: Rect) -> Rect {
        Rect::new(
            clamp_axis(self.x, self.w, area.x, area.w),
            clamp_axis(self.y, self.h, area.y, area.h),
            self.w,
            self.h,
        )
    }
}

fn clamp_axis(value: i32, length: i32, origin: i32, span: i32) -> i32 {
    if length >= span {
        return origin;
    }
    value.clamp(origin, origin + span - length)
}

/// What the measured size refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeBasis {
    /// Application content area (Sizer's `client`).
    Client,
    /// Whole window including decorations.
    Frame,
}

/// Measured geometry of one window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    pub frame: Rect,
    pub client: Rect,
    pub scale: f64,
    pub decorated: bool,
}

impl Geometry {
    pub fn rect(&self, basis: SizeBasis) -> Rect {
        match basis {
            SizeBasis::Client => self.client,
            SizeBasis::Frame => self.frame,
        }
    }

    /// Distance between the frame and client rectangles on each side.
    pub fn extents(&self) -> Extents {
        Extents {
            left: self.client.x - self.frame.x,
            top: self.client.y - self.frame.y,
            right: self.frame.right() - self.client.right(),
            bottom: self.frame.bottom() - self.client.bottom(),
        }
    }
}

/// Decoration thickness around the client area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Extents {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Extents {
    pub const ZERO: Self = Self {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };

    /// Convert a desired client rectangle into the frame rectangle the window
    /// manager has to be asked for.
    pub fn client_to_frame(&self, client: Rect) -> Rect {
        Rect::new(
            client.x - self.left,
            client.y - self.top,
            client.w + self.left + self.right,
            client.h + self.top + self.bottom,
        )
    }

    pub fn frame_to_client(&self, frame: Rect) -> Rect {
        Rect::new(
            frame.x + self.left,
            frame.y + self.top,
            frame.w - self.left - self.right,
            frame.h - self.top - self.bottom,
        )
    }
}

/// Outcome of applying a geometry request. Never reports success implicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeometryResult {
    pub requested: Rect,
    pub basis: SizeBasis,
    pub achieved: Rect,
    pub achieved_frame: Rect,
    pub achieved_client: Rect,
    pub scale: f64,
    /// The work area / monitor could not hold the request.
    pub clamped: bool,
    /// The request was left outside the area because the caller allowed it.
    pub outside: bool,
    /// The window landed exactly on the requested rect, in `basis` coordinates.
    pub exact: bool,
    /// The client's size hints or minimum size changed the request.
    pub adjusted_by_hints: bool,
    /// The size differs but neither the area nor the client's hints explain it.
    pub ignored: bool,
    /// Nothing moved or resized: the window manager ignored the request.
    pub unchanged: bool,
    /// The window was maximized and had to be restored first.
    pub unmaximized: bool,
}

impl GeometryResult {
    pub fn delta(&self) -> (i32, i32, i32, i32) {
        (
            self.achieved.x - self.requested.x,
            self.achieved.y - self.requested.y,
            self.achieved.w - self.requested.w,
            self.achieved.h - self.requested.h,
        )
    }

    pub fn is_exact(&self) -> bool {
        self.delta() == (0, 0, 0, 0)
    }
}

/// Requested size, resolved against a reference area at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SizeSpec {
    Pixels(i32, i32),
    Percent(f32, f32),
}

impl SizeSpec {
    /// Largest dimension a preset may ask for.
    ///
    /// Without a bound, `--percent 1e9x1e9` rounded to `i32::MAX` and then
    /// `x + w` wrapped to a negative coordinate, placing the window off-screen.
    pub const MAX_DIMENSION: i32 = 32768;
    /// Largest percentage of the reference area a preset may ask for.
    pub const MAX_PERCENT: f32 = 1000.0;

    /// Parse `1920x1080` or `50%x50%`. Both halves must use the same unit.
    pub fn parse(text: &str) -> Option<SizeSpec> {
        let (width, height) = text.trim().split_once(['x', 'X'])?;
        let width = width.trim();
        let height = height.trim();
        let (width, width_percent) = match width.strip_suffix('%') {
            Some(value) => (value.trim().parse::<f32>().ok()?, true),
            None => (width.parse::<f32>().ok()?, false),
        };
        let (height, height_percent) = match height.strip_suffix('%') {
            Some(value) => (value.trim().parse::<f32>().ok()?, true),
            None => (height.parse::<f32>().ok()?, false),
        };
        if width <= 0.0 || height <= 0.0 || width_percent != height_percent {
            return None;
        }
        if width_percent {
            if width > Self::MAX_PERCENT || height > Self::MAX_PERCENT {
                return None;
            }
            Some(SizeSpec::Percent(width, height))
        } else {
            if width > Self::MAX_DIMENSION as f32 || height > Self::MAX_DIMENSION as f32 {
                return None;
            }
            Some(SizeSpec::Pixels(width as i32, height as i32))
        }
    }

    pub fn resolve(&self, area: Rect) -> (i32, i32) {
        match *self {
            SizeSpec::Pixels(w, h) => (
                w.clamp(1, Self::MAX_DIMENSION),
                h.clamp(1, Self::MAX_DIMENSION),
            ),
            SizeSpec::Percent(fw, fh) => (
                ((area.w as f32 * fw / 100.0).round() as i32).clamp(1, Self::MAX_DIMENSION),
                ((area.h as f32 * fh / 100.0).round() as i32).clamp(1, Self::MAX_DIMENSION),
            ),
        }
    }
}

/// Where inside the reference area the window should end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
    /// Absolute offset from the area origin.
    Custom(i32, i32),
}

impl Anchor {
    pub const GRID: [Anchor; 9] = [
        Anchor::TopLeft,
        Anchor::Top,
        Anchor::TopRight,
        Anchor::Left,
        Anchor::Center,
        Anchor::Right,
        Anchor::BottomLeft,
        Anchor::Bottom,
        Anchor::BottomRight,
    ];

    pub fn place(&self, area: Rect, size: (i32, i32)) -> Rect {
        let (w, h) = size;
        let (x, y) = match *self {
            Anchor::TopLeft => (area.x, area.y),
            Anchor::Top => (area.center_x() - w / 2, area.y),
            Anchor::TopRight => (area.right() - w, area.y),
            Anchor::Left => (area.x, area.center_y() - h / 2),
            Anchor::Center => (area.center_x() - w / 2, area.center_y() - h / 2),
            Anchor::Right => (area.right() - w, area.center_y() - h / 2),
            Anchor::BottomLeft => (area.x, area.bottom() - h),
            Anchor::Bottom => (area.center_x() - w / 2, area.bottom() - h),
            Anchor::BottomRight => (area.right() - w, area.bottom() - h),
            Anchor::Custom(dx, dy) => (area.x + dx, area.y + dy),
        };
        Rect::new(x, y, w, h)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.to_ascii_lowercase().replace(['-', '_', ' '], "");
        Some(match normalized.as_str() {
            "topleft" | "tl" => Anchor::TopLeft,
            "top" | "t" => Anchor::Top,
            "topright" | "tr" => Anchor::TopRight,
            "left" | "l" => Anchor::Left,
            "center" | "centre" | "c" => Anchor::Center,
            "right" | "r" => Anchor::Right,
            "bottomleft" | "bl" => Anchor::BottomLeft,
            "bottom" | "b" => Anchor::Bottom,
            "bottomright" | "br" => Anchor::BottomRight,
            _ => return None,
        })
    }
}

/// Which screen area geometry is resolved against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaKind {
    /// Usable area minus panels/docks.
    #[serde(alias = "workarea", alias = "work")]
    WorkArea,
    /// Full monitor rectangle.
    Monitor,
}

/// Which monitor to target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorSelector {
    /// The monitor under the pointer, identically on every backend.
    Active,
    Primary,
    Index(usize),
}

/// Opaque, backend-specific window handle. Must stay stable for the session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything the UI/CLI needs to show about one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub app_id: Option<String>,
    pub pid: Option<u32>,
    pub geometry: Geometry,
    pub maximized: bool,
    pub above: bool,
    pub focused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 32, 1600, 868)
    }

    #[test]
    fn anchor_grid_is_pixel_exact() {
        assert_eq!(
            Anchor::TopLeft.place(area(), (800, 600)),
            Rect::new(0, 32, 800, 600)
        );
        assert_eq!(
            Anchor::Top.place(area(), (800, 600)),
            Rect::new(400, 32, 800, 600)
        );
        assert_eq!(
            Anchor::TopRight.place(area(), (800, 600)),
            Rect::new(800, 32, 800, 600)
        );
        assert_eq!(
            Anchor::Left.place(area(), (800, 600)),
            Rect::new(0, 166, 800, 600)
        );
        assert_eq!(
            Anchor::Center.place(area(), (800, 600)),
            Rect::new(400, 166, 800, 600)
        );
        assert_eq!(
            Anchor::Right.place(area(), (800, 600)),
            Rect::new(800, 166, 800, 600)
        );
        assert_eq!(
            Anchor::BottomLeft.place(area(), (800, 600)),
            Rect::new(0, 300, 800, 600)
        );
        assert_eq!(
            Anchor::Bottom.place(area(), (800, 600)),
            Rect::new(400, 300, 800, 600)
        );
        assert_eq!(
            Anchor::BottomRight.place(area(), (800, 600)),
            Rect::new(800, 300, 800, 600)
        );
    }

    #[test]
    fn right_anchor_touches_work_area_edge() {
        let placed = Anchor::BottomRight.place(area(), (500, 400));
        assert_eq!(placed.right(), area().right());
        assert_eq!(placed.bottom(), area().bottom());
    }

    #[test]
    fn percent_size_resolves_against_area() {
        assert_eq!(SizeSpec::Percent(50.0, 50.0).resolve(area()), (800, 434));
        assert_eq!(SizeSpec::Pixels(800, 600).resolve(area()), (800, 600));
    }

    #[test]
    fn sizes_are_bounded_before_they_can_wrap() {
        assert!(SizeSpec::parse("40000x100").is_none());
        assert!(SizeSpec::parse("100x40000").is_none());
        assert!(SizeSpec::parse("2000%x50%").is_none());
        assert_eq!(
            SizeSpec::parse("32768x32768"),
            Some(SizeSpec::Pixels(32768, 32768))
        );
        // Resolution clamps, so a programmatic value cannot overflow `x + w`.
        assert_eq!(
            SizeSpec::Percent(1.0e9, 1.0e9).resolve(area()),
            (SizeSpec::MAX_DIMENSION, SizeSpec::MAX_DIMENSION)
        );
        assert_eq!(
            SizeSpec::Pixels(i32::MAX, i32::MAX).resolve(area()),
            (SizeSpec::MAX_DIMENSION, SizeSpec::MAX_DIMENSION)
        );
    }

    #[test]
    fn extents_round_trip_frame_and_client() {
        let extents = Extents {
            left: 0,
            top: 37,
            right: 0,
            bottom: 0,
        };
        let frame = Rect::new(400, 100, 900, 737);
        let client = extents.frame_to_client(frame);
        assert_eq!(client, Rect::new(400, 137, 900, 700));
        assert_eq!(extents.client_to_frame(client), frame);
    }

    #[test]
    fn custom_anchor_is_area_relative() {
        assert_eq!(
            Anchor::Custom(24, 48).place(area(), (100, 100)),
            Rect::new(24, 80, 100, 100)
        );
    }

    #[test]
    fn anchor_parsing_accepts_short_and_spaced_forms() {
        assert_eq!(Anchor::parse("top-right"), Some(Anchor::TopRight));
        assert_eq!(Anchor::parse("BR"), Some(Anchor::BottomRight));
        assert_eq!(Anchor::parse("nowhere"), None);
    }

    #[test]
    fn geometry_result_reports_real_delta() {
        let result = GeometryResult {
            requested: Rect::new(100, 100, 1024, 768),
            basis: SizeBasis::Frame,
            achieved: Rect::new(100, 100, 1024, 768),
            achieved_frame: Rect::new(100, 100, 1024, 768),
            achieved_client: Rect::new(86, 88, 1052, 797),
            scale: 1.0,
            clamped: false,
            outside: false,
            exact: true,
            adjusted_by_hints: false,
            ignored: false,
            unchanged: false,
            unmaximized: false,
        };
        assert!(result.is_exact());
    }
}
