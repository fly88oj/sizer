//! Cursor-anchored preset menu drawn with the X11 core protocol.
//!
//! GTK4 cannot position a top-level window on X11 (GdkX11-4.0 exposes no surface
//! move/position API, verified in Phase 1), so the menu is a plain
//! override-redirect window owned by this process. No toolkit or compositor is
//! involved, which keeps the menu usable from the daemon as well as from the
//! CLI.
//!
//! Text is drawn through Xft: the core font path only covers Latin-1, so preset
//! labels with CJK characters came out as `?`. Xft draws UTF-8 with the font
//! fontconfig picks for the characters actually used, and measures the same
//! string for layout and truncation.
//!
//! The menu grabs the pointer and keyboard on the root window before it becomes
//! visible: that is what makes an outside click and Escape dismiss it. Sessions
//! that keep both devices grabbed themselves therefore never see the menu.
//! GNOME's mutter holds such a grab itself: the e2e harness still gets
//! `ALREADY_GRABBED` 25s into a mutter session, and the menu's 40 x 25ms retry
//! gives up after about a second. That is why GNOME's own menu lives in the
//! shell extension.

use std::thread::sleep;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, GrabStatus, PropMode,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use sizer_config::menu::MenuNode;
use sizer_core::Rect;
use sizer_platform::{BackendError, BackendResult};

mod xft;

use xft::{Ink, TextPainter};

/// Drawn border thickness inside the window.
const BORDER: i32 = 1;
/// Space between the border and the rows, and left/right text inset.
const PADDING: i32 = 8;
/// Space above and below the text inside one row.
const ROW_PADDING: i32 = 5;
/// Widest label area in pixels; longer labels are truncated with an ellipsis.
const MAX_TEXT_WIDTH: i32 = 420;
/// Appended to labels that do not fit [`MAX_TEXT_WIDTH`].
const ELLIPSIS: &str = "\u{2026}";
/// Appended to group rows; the submenu affordance must survive truncation.
const GROUP_SUFFIX: &str = " >";
/// How often the pointer/keyboard grab is retried while another client holds it.
const GRAB_ATTEMPTS: usize = 40;
const GRAB_RETRY: Duration = Duration::from_millis(25);

const KEYSYM_ESCAPE: u32 = 0xff1b;
const KEYSYM_RETURN: u32 = 0xff0d;
const KEYSYM_KP_ENTER: u32 = 0xff8d;
const KEYSYM_HOME: u32 = 0xff50;
const KEYSYM_END: u32 = 0xff57;
const KEYSYM_LEFT: u32 = 0xff51;
const KEYSYM_UP: u32 = 0xff52;
const KEYSYM_RIGHT: u32 = 0xff53;
const KEYSYM_DOWN: u32 = 0xff54;
const KEYSYM_DIGIT_FIRST: u32 = 0x31;
const KEYSYM_DIGIT_LAST: u32 = 0x39;

/// Text metrics the layout needs. Also the seam for unit tests: no X server is
/// involved in placing the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuMetrics {
    pub ascent: i32,
    pub descent: i32,
    /// Rendered width of every entry, in pixels.
    pub widths: Vec<i32>,
}

/// Where the menu and its rows end up, in root coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLayout {
    pub rect: Rect,
    pub row_height: i32,
    pub rows: Vec<Rect>,
}

/// Place a menu of `metrics.widths.len()` rows at the pointer without leaving
/// `area`.
pub fn layout(metrics: &MenuMetrics, anchor: (i32, i32), area: Rect) -> MenuLayout {
    let text_width = metrics.widths.iter().copied().max().unwrap_or(0);
    let row_height = metrics.ascent + metrics.descent + 2 * ROW_PADDING;
    // The window itself never leaves the area. Rows are paged before this point
    // (`page_nodes`), so this is the guard for pathological metrics, not the
    // mechanism that keeps the menu usable.
    let width = (text_width + 2 * PADDING + 2 * BORDER).min(area.w);
    let height = (row_height * metrics.widths.len() as i32 + 2 * PADDING + 2 * BORDER).min(area.h);
    let rect = Rect::new(
        anchor.0.min(area.right() - width).max(area.x),
        anchor.1.min(area.bottom() - height).max(area.y),
        width,
        height,
    );
    let rows = (0..metrics.widths.len())
        .map(|index| {
            Rect::new(
                rect.x + BORDER,
                rect.y + BORDER + PADDING + index as i32 * row_height,
                rect.w - 2 * BORDER,
                row_height,
            )
        })
        .collect();
    MenuLayout {
        rect,
        row_height,
        rows,
    }
}

/// Keep a level inside `max_rows` rows by moving the overflow into one submenu.
///
/// Depth-first order is preserved, so the preset indices a click returns are
/// unchanged; a menu taller than the screen becomes one extra level instead of
/// rows that are drawn outside the screen and cannot be clicked.
fn page_nodes(nodes: Vec<MenuNode>, max_rows: usize) -> Vec<MenuNode> {
    let max_rows = max_rows.max(2);
    if nodes.len() <= max_rows {
        return nodes;
    }
    let keep = max_rows - 1;
    let rest = nodes[keep..].to_vec();
    let mut page = nodes[..keep].to_vec();
    page.push(MenuNode::Group {
        label: format!("\u{2026} {} more", rest.len()),
        children: rest,
    });
    page
}

/// Show the preset menu at `anchor` and return the chosen preset index.
///
/// Groups open a cascading submenu to the right of their row. `None` means the
/// menu was dismissed: Escape, a click outside every level, or a broken pointer
/// grab. The call blocks until one of those happens.
pub fn show_menu(
    entries: &[MenuNode],
    anchor: (i32, i32),
    area: Rect,
) -> BackendResult<Option<usize>> {
    if entries.is_empty() {
        return Err(BackendError::Protocol(
            "the preset menu has no entries".to_string(),
        ));
    }
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|error| BackendError::Unavailable(format!("cannot open X display: {error}")))?;
    let screen = &conn.setup().roots[screen_num];

    let mut text = TextPainter::load(&labels(entries))?;
    let text_limit = MAX_TEXT_WIDTH
        .min(area.w - 2 * PADDING - 2 * BORDER)
        .max(40);

    // Arm input before anything becomes visible: a menu that cannot grab the
    // pointer cannot close itself on an outside click, and a window that appears
    // first would swallow clicks for as long as the retry takes.
    let grab = GrabGuard::take(&conn, screen.root)?;
    let mut menu = Menu {
        conn: &conn,
        root: screen.root,
        background: screen.black_pixel,
        area,
        text_limit,
        levels: Vec::new(),
    };
    menu.push_level(entries.to_vec(), anchor, &text)?;
    menu.render(&mut text)?;

    let outcome = menu.run(&mut text);
    // The painter holds an XftDraw (and thus a Picture) on the newest window;
    // destroying a drawable frees its Picture, so unbind first.
    text.detach();
    for level in &menu.levels {
        let _ = conn.destroy_window(level.win);
    }
    drop(grab);
    drop(text);
    let _ = conn.flush();
    outcome
}

/// Every line the menu can show, so one font covers all levels.
fn labels(entries: &[MenuNode]) -> Vec<String> {
    let mut lines = Vec::new();
    collect_labels(entries, &mut lines);
    lines
}

fn collect_labels(entries: &[MenuNode], lines: &mut Vec<String>) {
    for entry in entries {
        match entry {
            MenuNode::Preset { label, .. } => lines.push(label.clone()),
            // A separator occupies one row; it draws a line instead of text.
            MenuNode::Separator { .. } => lines.push(String::new()),
            MenuNode::Group { label, children } => {
                lines.push(format!("{label}{GROUP_SUFFIX}"));
                collect_labels(children, lines);
            }
        }
    }
}

/// One line as drawn: groups keep their submenu arrow when truncated.
fn display_line(node: &MenuNode, limit: i32, measure: impl Fn(&str) -> i32) -> String {
    match node {
        MenuNode::Preset { label, .. } => truncate(label, limit, |text| measure(text)),
        MenuNode::Separator { .. } => String::new(),
        MenuNode::Group { label, .. } => {
            let budget = (limit - measure(GROUP_SUFFIX)).max(40);
            format!(
                "{}{GROUP_SUFFIX}",
                truncate(label, budget, |text| measure(text))
            )
        }
    }
}

/// Anchor for a submenu so its first row lines up with the group row.
fn submenu_anchor(parent: &MenuLayout, row: usize) -> (i32, i32) {
    let row = parent.rows[row];
    (parent.rect.right() - BORDER, row.y - PADDING - BORDER)
}

/// One level of the menu: the root level plus one per open submenu.
struct Level {
    nodes: Vec<MenuNode>,
    win: Window,
    lines: Vec<String>,
    placement: MenuLayout,
    selected: Option<usize>,
}

/// The open menu: all levels, their windows and the event loop.
struct Menu<'a> {
    conn: &'a RustConnection,
    root: Window,
    background: u32,
    area: Rect,
    text_limit: i32,
    levels: Vec<Level>,
}

impl Menu<'_> {
    /// Open one more level at `anchor` (root first, then submenus).
    fn push_level(
        &mut self,
        nodes: Vec<MenuNode>,
        anchor: (i32, i32),
        text: &TextPainter,
    ) -> BackendResult<()> {
        let row_height = text.ascent() + text.descent() + 2 * ROW_PADDING;
        let max_rows = ((self.area.h - 2 * PADDING - 2 * BORDER) / row_height).max(2) as usize;
        let nodes = page_nodes(nodes, max_rows);
        let lines: Vec<String> = nodes
            .iter()
            .map(|node| display_line(node, self.text_limit, |line| text.width(line)))
            .collect();
        let metrics = MenuMetrics {
            ascent: text.ascent(),
            descent: text.descent(),
            widths: lines.iter().map(|line| text.width(line)).collect(),
        };
        let placement = layout(&metrics, anchor, self.area);
        let win = create_window(self.conn, self.root, self.background, &placement)?;
        set_properties(self.conn, win)?;
        self.conn
            .map_window(win)
            .map_err(|error| BackendError::Protocol(error.to_string()))?;
        self.conn
            .flush()
            .map_err(|error| BackendError::Protocol(error.to_string()))?;
        wait_for_map(self.conn)?;
        self.levels.push(Level {
            nodes,
            win,
            lines,
            placement,
            selected: None,
        });
        Ok(())
    }

    /// Drop every level above `keep`.
    fn pop_to(&mut self, keep: usize, text: &mut TextPainter) {
        if self.levels.len() > keep + 1 {
            // Same drawable/Picture rule as in `show_menu`.
            text.detach();
        }
        while self.levels.len() > keep + 1 {
            if let Some(level) = self.levels.pop() {
                let _ = self.conn.destroy_window(level.win);
            }
        }
    }

    /// Deepest level and row under a root-relative point.
    fn hit(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        for depth in (0..self.levels.len()).rev() {
            if let Some(row) = row_at_point(&self.levels[depth].placement, x, y) {
                return Some((depth, row));
            }
        }
        None
    }

    fn render(&mut self, text: &mut TextPainter) -> BackendResult<()> {
        for level in &self.levels {
            let rect = level.placement.rect;
            text.attach(level.win)?;
            text.fill(Ink::Background, 0, 0, rect.w, rect.h);
            text.fill(Ink::Border, 0, 0, rect.w, BORDER);
            text.fill(Ink::Border, 0, rect.h - BORDER, rect.w, BORDER);
            text.fill(Ink::Border, 0, 0, BORDER, rect.h);
            text.fill(Ink::Border, rect.w - BORDER, 0, BORDER, rect.h);
            if let Some(index) = level.selected {
                // A separator is never a valid selection; a stray one would only
                // draw a highlight over a line.
                if !matches!(level.nodes[index], MenuNode::Separator { .. }) {
                    let row = level.placement.rows[index];
                    text.fill(Ink::Accent, row.x - rect.x, row.y - rect.y, row.w, row.h);
                }
            }
            for (index, line) in level.lines.iter().enumerate() {
                let row = level.placement.rows[index];
                if matches!(level.nodes[index], MenuNode::Separator { .. }) {
                    let y = row.y - rect.y + row.h / 2;
                    text.fill(
                        Ink::Border,
                        row.x - rect.x + PADDING,
                        y,
                        row.w - 2 * PADDING,
                        1,
                    );
                    continue;
                }
                let ink = if level.selected == Some(index) {
                    Ink::SelectedText
                } else {
                    Ink::Text
                };
                text.draw_text(
                    ink,
                    row.x - rect.x + PADDING,
                    row.y - rect.y + ROW_PADDING + text.ascent(),
                    line,
                );
            }
        }
        text.flush();
        Ok(())
    }

    /// Run a row: presets return their index, groups open their submenu.
    fn activate(
        &mut self,
        depth: usize,
        row: usize,
        text: &mut TextPainter,
    ) -> BackendResult<Option<usize>> {
        let Some(node) = self.levels[depth].nodes.get(row).cloned() else {
            return Ok(None);
        };
        match node {
            MenuNode::Preset { index, .. } => Ok(Some(index)),
            MenuNode::Separator { .. } => Ok(None),
            MenuNode::Group { children, .. } => {
                self.levels[depth].selected = Some(row);
                self.pop_to(depth, text);
                let anchor = submenu_anchor(&self.levels[depth].placement, row);
                self.push_level(children, anchor, text)?;
                // The deepest level starts with no selection, like a fresh menu.
                self.render(text)?;
                Ok(None)
            }
        }
    }

    fn move_selection(&mut self, delta: i32, text: &mut TextPainter) -> BackendResult<()> {
        let Some(level) = self.levels.last_mut() else {
            return Ok(());
        };
        let count = level.lines.len();
        if count == 0 {
            return Ok(());
        }
        let start = match level.selected {
            Some(index) => index as i32,
            None if delta >= 0 => -1,
            None => count as i32,
        };
        // Walk past separators: they are rows, but never a selection.
        for step in 1..=count {
            let index = (start + delta * step as i32).rem_euclid(count as i32) as usize;
            if !matches!(level.nodes[index], MenuNode::Separator { .. }) {
                level.selected = Some(index);
                return self.render(text);
            }
        }
        Ok(())
    }

    fn select(&mut self, index: usize, text: &mut TextPainter) -> BackendResult<()> {
        if let Some(level) = self.levels.last_mut() {
            if !matches!(level.nodes[index], MenuNode::Separator { .. }) {
                level.selected = Some(index);
            }
        }
        self.render(text)
    }

    fn run(&mut self, text: &mut TextPainter) -> BackendResult<Option<usize>> {
        let mut keysym_map = keyboard_map(self.conn)?;
        loop {
            let event = self
                .conn
                .wait_for_event()
                .map_err(|error| BackendError::Protocol(error.to_string()))?;
            match event {
                Event::MappingNotify(_) => keysym_map = keyboard_map(self.conn)?,
                Event::Expose(expose) if expose.count == 0 => self.render(text)?,
                Event::MotionNotify(motion) => {
                    let (x, y) = (motion.root_x as i32, motion.root_y as i32);
                    let Some((depth, row)) = self.hit(x, y) else {
                        continue;
                    };
                    // Moving back onto a parent row closes the submenu below it.
                    if depth + 1 < self.levels.len() && self.levels[depth].selected != Some(row) {
                        self.pop_to(depth, text);
                    }
                    if self.levels[depth].selected != Some(row) {
                        if matches!(self.levels[depth].nodes[row], MenuNode::Separator { .. }) {
                            continue;
                        }
                        self.levels[depth].selected = Some(row);
                        self.render(text)?;
                    }
                }
                Event::ButtonRelease(release) => {
                    let (x, y) = (release.root_x as i32, release.root_y as i32);
                    // A click outside every level dismisses the whole menu.
                    let Some((depth, row)) = self.hit(x, y) else {
                        return Ok(None);
                    };
                    self.pop_to(depth, text);
                    if let Some(index) = self.activate(depth, row, text)? {
                        return Ok(Some(index));
                    }
                }
                Event::KeyPress(press) => {
                    let Some(keysym) = keysym_for(&keysym_map, press.detail) else {
                        continue;
                    };
                    let depth = self.levels.len() - 1;
                    let selected = self.levels[depth].selected;
                    match keysym {
                        KEYSYM_ESCAPE | KEYSYM_LEFT => {
                            if depth == 0 {
                                return Ok(None);
                            }
                            self.pop_to(depth - 1, text);
                            self.render(text)?;
                        }
                        KEYSYM_RETURN | KEYSYM_KP_ENTER | KEYSYM_RIGHT => {
                            let Some(row) = selected else {
                                continue;
                            };
                            if keysym == KEYSYM_RIGHT
                                && !matches!(self.levels[depth].nodes[row], MenuNode::Group { .. })
                            {
                                continue;
                            }
                            if let Some(index) = self.activate(depth, row, text)? {
                                return Ok(Some(index));
                            }
                        }
                        KEYSYM_UP => self.move_selection(-1, text)?,
                        KEYSYM_DOWN => self.move_selection(1, text)?,
                        KEYSYM_HOME => self.select(0, text)?,
                        KEYSYM_END => self.select(self.levels[depth].lines.len() - 1, text)?,
                        KEYSYM_DIGIT_FIRST..=KEYSYM_DIGIT_LAST => {
                            let row = (keysym - KEYSYM_DIGIT_FIRST) as usize;
                            if row >= self.levels[depth].lines.len() {
                                continue;
                            }
                            self.select(row, text)?;
                            if let Some(index) = self.activate(depth, row, text)? {
                                return Ok(Some(index));
                            }
                        }
                        _ => {}
                    }
                }
                Event::Error(error) => {
                    return Err(BackendError::Protocol(format!("X error: {error:?}")));
                }
                _ => {}
            }
        }
    }
}

/// Create one of the menu windows at `placement`.
fn create_window(
    conn: &RustConnection,
    parent: Window,
    background: u32,
    placement: &MenuLayout,
) -> BackendResult<Window> {
    let win = conn
        .generate_id()
        .map_err(|error| BackendError::Protocol(error.to_string()))?;
    let aux = CreateWindowAux::new()
        .background_pixel(background)
        .override_redirect(1)
        .save_under(1)
        // Pointer and key events arrive through the root grab, so only exposure
        // and mapping are selected here.
        .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);
    conn.create_window(
        0,
        win,
        parent,
        placement.rect.x as i16,
        placement.rect.y as i16,
        placement.rect.w as u16,
        placement.rect.h as u16,
        0,
        WindowClass::COPY_FROM_PARENT,
        0,
        &aux,
    )
    .map_err(|error| BackendError::Protocol(error.to_string()))?;
    Ok(win)
}

/// Mark a menu window as a popup menu owned by Sizer.
fn set_properties(conn: &RustConnection, win: Window) -> BackendResult<()> {
    let window_type = intern(conn, "_NET_WM_WINDOW_TYPE")?;
    let popup = intern(conn, "_NET_WM_WINDOW_TYPE_POPUP_MENU")?;
    let fail = |error: x11rb::errors::ConnectionError| BackendError::Protocol(error.to_string());
    conn.change_property32(
        PropMode::REPLACE,
        win,
        window_type,
        AtomEnum::ATOM,
        &[popup],
    )
    .map_err(fail)?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"sizer\0Sizer\0",
    )
    .map_err(fail)?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Sizer",
    )
    .map_err(fail)?;
    Ok(())
}

/// Longest prefix of `text` that fits `limit` once [`ELLIPSIS`] is appended.
///
/// Truncation walks characters, never bytes, so a label is never split inside a
/// multi-byte character.
fn truncate(text: &str, limit: i32, measure: impl Fn(&str) -> i32) -> String {
    if measure(text) <= limit {
        return text.to_string();
    }
    let ellipsis = measure(ELLIPSIS);
    let mut prefix = String::new();
    for character in text.chars() {
        let mut candidate = prefix.clone();
        candidate.push(character);
        if measure(&candidate) + ellipsis > limit {
            break;
        }
        prefix = candidate;
    }
    prefix.push_str(ELLIPSIS);
    prefix
}

/// Take the pointer and keyboard on the root window, retrying while another
/// client holds them.
///
/// The shell can hold both devices while it runs a grab op (startup, drag,
/// gesture). Retrying keeps the menu usable through that, and a persistent
/// owner is reported instead of opening a menu that cannot be dismissed.
fn grab_input(conn: &RustConnection, root: Window) -> BackendResult<()> {
    let mut pointer = GrabStatus::ALREADY_GRABBED;
    for attempt in 0..GRAB_ATTEMPTS {
        if attempt > 0 {
            sleep(GRAB_RETRY);
        }
        pointer = conn
            .grab_pointer(
                false,
                root,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )
            .map_err(|error| BackendError::Protocol(error.to_string()))?
            .reply()
            .map_err(|error| BackendError::Protocol(error.to_string()))?
            .status;
        if pointer == GrabStatus::SUCCESS {
            break;
        }
    }
    if pointer != GrabStatus::SUCCESS {
        return Err(BackendError::Protocol(format!(
            "another client holds the pointer (grab status {pointer:?} after {GRAB_ATTEMPTS} attempts); \
             GNOME holds the devices itself, use the shell extension menu there"
        )));
    }
    // Without the keyboard grab Escape would go to the focused window.
    let mut keyboard = GrabStatus::ALREADY_GRABBED;
    for attempt in 0..GRAB_ATTEMPTS {
        if attempt > 0 {
            sleep(GRAB_RETRY);
        }
        keyboard = conn
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|error| BackendError::Protocol(error.to_string()))?
            .reply()
            .map_err(|error| BackendError::Protocol(error.to_string()))?
            .status;
        if keyboard == GrabStatus::SUCCESS {
            break;
        }
    }
    if keyboard != GrabStatus::SUCCESS {
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
        return Err(BackendError::Protocol(format!(
            "another client holds the keyboard (grab status {keyboard:?} after {GRAB_ATTEMPTS} attempts); \
             GNOME keeps the devices grabbed for the whole session, use the shell extension menu there"
        )));
    }
    conn.flush()
        .map_err(|error| BackendError::Protocol(error.to_string()))
}

/// Pointer and keyboard grabs on the root window, released when dropped.
struct GrabGuard<'a> {
    conn: &'a RustConnection,
}

impl<'a> GrabGuard<'a> {
    fn take(conn: &'a RustConnection, root: Window) -> BackendResult<Self> {
        grab_input(conn, root)?;
        Ok(Self { conn })
    }
}

impl Drop for GrabGuard<'_> {
    fn drop(&mut self) {
        // Best effort: the process is about to exit or already has the answer.
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }
}

fn wait_for_map(conn: &RustConnection) -> BackendResult<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match conn
            .poll_for_event()
            .map_err(|error| BackendError::Protocol(error.to_string()))?
        {
            Some(Event::MapNotify(_)) => return Ok(()),
            Some(_) => {}
            None if Instant::now() < deadline => sleep(Duration::from_millis(10)),
            None => {
                return Err(BackendError::Protocol(
                    "the menu window was never mapped".to_string(),
                ));
            }
        }
    }
}

fn intern(conn: &RustConnection, name: &str) -> BackendResult<u32> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(|error| BackendError::Protocol(error.to_string()))?
        .reply()
        .map_err(|error| BackendError::Protocol(error.to_string()))?
        .atom)
}

fn keysym_for(map: &KeysymMap, keycode: u8) -> Option<u32> {
    let index = (keycode.wrapping_sub(map.min_keycode)) as usize * map.per_keycode;
    map.keysyms
        .get(index)
        .copied()
        .filter(|keysym| *keysym != 0)
}

struct KeysymMap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

fn keyboard_map(conn: &RustConnection) -> BackendResult<KeysymMap> {
    let setup = conn.setup();
    let min_keycode = setup.min_keycode;
    let count = setup.max_keycode - min_keycode + 1;
    let reply = conn
        .get_keyboard_mapping(min_keycode, count)
        .map_err(|error| BackendError::Protocol(error.to_string()))?
        .reply()
        .map_err(|error| BackendError::Protocol(error.to_string()))?;
    Ok(KeysymMap {
        min_keycode,
        per_keycode: usize::from(reply.keysyms_per_keycode),
        keysyms: reply.keysyms,
    })
}

/// Row under a root-relative point.
///
/// The grab is taken on the root window, so grabbed pointer events report root
/// coordinates; that is also what the layout uses.
fn row_at_point(placement: &MenuLayout, x: i32, y: i32) -> Option<usize> {
    if x < placement.rect.x || x >= placement.rect.right() {
        return None;
    }
    placement
        .rows
        .iter()
        .position(|row| y >= row.y && y < row.bottom())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(widths: &[i32]) -> MenuMetrics {
        MenuMetrics {
            ascent: 11,
            descent: 3,
            widths: widths.to_vec(),
        }
    }

    fn area() -> Rect {
        Rect::new(0, 32, 1600, 868)
    }

    #[test]
    fn menu_opens_with_its_corner_at_the_pointer() {
        let placement = layout(&metrics(&[100, 140]), (400, 300), area());
        assert_eq!(placement.rect.x, 400);
        assert_eq!(placement.rect.y, 300);
        assert_eq!(placement.rect.w, 140 + 2 * PADDING + 2 * BORDER);
        // Two rows of 11+3+10 pixels plus the surrounding padding.
        assert_eq!(placement.rect.h, 2 * 24 + 2 * PADDING + 2 * BORDER);
        assert_eq!(placement.row_height, 24);
    }

    #[test]
    fn rows_are_stacked_in_window_order() {
        let placement = layout(&metrics(&[100, 140, 60]), (10, 40), area());
        let first = placement.rows[0];
        let second = placement.rows[1];
        assert_eq!(first.y, placement.rect.y + BORDER + PADDING);
        assert_eq!(second.y, first.y + placement.row_height);
        assert_eq!(placement.rows[2].y, second.y + placement.row_height);
        assert_eq!(first.w, placement.rect.w - 2 * BORDER);
    }

    #[test]
    fn menu_flips_inside_the_area_near_the_bottom_right_corner() {
        let placement = layout(&metrics(&[200, 200]), (1590, 890), area());
        assert_eq!(placement.rect.right(), area().right());
        assert_eq!(placement.rect.bottom(), area().bottom());
        assert!(placement.rect.x <= 1590);
        assert!(placement.rect.y <= 890);
    }

    #[test]
    fn menu_is_clamped_to_the_area_origin() {
        let placement = layout(&metrics(&[80]), (-20, 10), area());
        assert_eq!(placement.rect.x, area().x);
        assert_eq!(placement.rect.y, area().y);
    }

    #[test]
    fn menu_height_never_exceeds_the_area() {
        let widths = vec![100; 60];
        let tall = layout(&metrics(&widths), (10, 40), Rect::new(0, 0, 800, 400));
        assert_eq!(tall.rect.h, 400);
        assert!(tall.rect.bottom() <= 400);
    }

    #[test]
    fn overflow_rows_move_into_a_more_submenu_in_order() {
        let nodes: Vec<MenuNode> = (0..25)
            .map(|index| MenuNode::Preset {
                index,
                label: format!("p{index}"),
            })
            .collect();
        let page = page_nodes(nodes, 10);
        assert_eq!(page.len(), 10);
        let MenuNode::Group { label, children } = &page[9] else {
            panic!("the last row of a paged menu is the overflow group");
        };
        assert_eq!(label, "\u{2026} 16 more");
        assert_eq!(children.len(), 16);
        // Depth-first order is unchanged: clicking still returns the same
        // configured preset indices.
        let mut indices = Vec::new();
        fn walk(nodes: &[MenuNode], indices: &mut Vec<usize>) {
            for node in nodes {
                match node {
                    MenuNode::Preset { index, .. } => indices.push(*index),
                    MenuNode::Group { children, .. } => walk(children, indices),
                    MenuNode::Separator { .. } => {}
                }
            }
        }
        walk(&page, &mut indices);
        assert_eq!(indices, (0..25).collect::<Vec<_>>());
        // The overflow group is paged again when the area is still too small.
        assert_eq!(page_nodes(children.clone(), 6).len(), 6);
    }

    #[test]
    fn a_page_that_fits_is_left_alone() {
        let nodes: Vec<MenuNode> = (0..8)
            .map(|index| MenuNode::Preset {
                index,
                label: format!("p{index}"),
            })
            .collect();
        assert_eq!(page_nodes(nodes, 8).len(), 8);
    }

    #[test]
    fn row_hit_testing_uses_half_open_intervals() {
        let placement = layout(&metrics(&[100, 100]), (0, 40), area());
        let first = placement.rows[0];
        let x = first.x + 1;
        assert_eq!(row_at_point(&placement, x, first.y), Some(0));
        assert_eq!(row_at_point(&placement, x, first.bottom() - 1), Some(0));
        assert_eq!(row_at_point(&placement, x, first.bottom()), Some(1));
        // Border and padding absorb clicks: those dismiss the menu.
        assert_eq!(row_at_point(&placement, x, placement.rect.y), None);
        assert_eq!(row_at_point(&placement, x, first.y - 1), None);
        assert_eq!(row_at_point(&placement, x, placement.rect.bottom()), None);
        // The x axis is part of the hit test: a neighbour window is not ours,
        // while the full inner width of a row band counts as that row.
        assert_eq!(
            row_at_point(&placement, placement.rect.x - 1, first.y + 1),
            None
        );
        assert_eq!(
            row_at_point(&placement, placement.rect.right(), first.y + 1),
            None
        );
        assert_eq!(
            row_at_point(&placement, first.right(), first.y + 1),
            Some(0)
        );
    }

    #[test]
    fn submenu_anchor_lines_up_with_its_row() {
        let parent = layout(&metrics(&[100, 120]), (400, 300), area());
        let anchor = submenu_anchor(&parent, 1);
        assert_eq!(anchor.0, parent.rect.right() - BORDER);
        assert_eq!(anchor.1, parent.rows[1].y - PADDING - BORDER);
        // Opening there puts the first row exactly beside the clicked row.
        let child = layout(&metrics(&[80, 90]), anchor, area());
        assert_eq!(child.rect.x, parent.rect.right() - BORDER);
        assert_eq!(child.rows[0].y, parent.rows[1].y);
    }

    #[test]
    fn group_rows_keep_their_arrow() {
        let group = MenuNode::Group {
            label: "16:9".to_string(),
            children: vec![],
        };
        assert_eq!(display_line(&group, 200, fake_width), "16:9 >");
        let preset = MenuNode::Preset {
            index: 0,
            label: "1024x768".to_string(),
        };
        assert_eq!(display_line(&preset, 200, fake_width), "1024x768");
    }

    #[test]
    fn truncation_never_drops_the_group_arrow() {
        let group = MenuNode::Group {
            label: "a very long group name".to_string(),
            children: vec![],
        };
        let line = display_line(&group, 80, fake_width);
        assert!(line.ends_with(" >"));
        assert!(line.ends_with(&format!("{ELLIPSIS}{GROUP_SUFFIX}")));
    }

    /// 8 pixels per character, so the expected strings below are readable.
    fn fake_width(text: &str) -> i32 {
        text.chars().count() as i32 * 8
    }

    #[test]
    fn truncate_keeps_labels_that_fit() {
        assert_eq!(truncate("1024x768", 200, fake_width), "1024x768");
        // 19 chars × 8px = 152px; budget 200 comfortably fits.
        assert_eq!(
            truncate("Half Screen 640x480", 200, fake_width),
            "Half Screen 640x480"
        );
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // 19 characters; budget 64 fits 8 of them, with the last replaced by
        // the ellipsis — so 7 source chars + ellipsis = 8 total.
        assert_eq!(
            truncate("Half Screen 640x480", 64, fake_width),
            "Half Sc\u{2026}"
        );
        assert_eq!(
            truncate("Half Screen 640x480", 64, fake_width)
                .chars()
                .count(),
            8
        );
    }

    #[test]
    fn truncate_never_splits_a_character() {
        let result = truncate("Window Size Setting", 20, fake_width);
        assert!(result.ends_with('\u{2026}'));
        assert_eq!(result, "W\u{2026}");
        // Still valid UTF-8 with no replacement characters.
        assert!(!result.contains('\u{fffd}'));
    }
}
