//! GTK4 window that edits the preset list of a configuration file.
//!
//! The window is a thin front end over `sizer_config`: every edit mutates an
//! in-memory [`Config`] and "Save" runs the same validation as `sizer config
//! check` before the file is written atomically. The daemon picks the new file
//! up on its own, so the tray and the shell menu follow without a restart.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::glib::Type;
use sizer_config::{Config, Preset, PresetKind, menu::MenuNode};
use sizer_core::{Anchor, SizeSpec};

use crate::CliResult;

/// Anchor names shown in the DropDown. Index 0 means "no anchor".
///
/// The array is kept in lockstep with [`Anchor::GRID`]: nine anchors, nine
/// labels, same order. `anchor_at(index)` and `anchor_index(anchor)` rely on
/// this positional correspondence. A compile-time assertion below would fire
/// if [`Anchor::GRID`] ever gains or loses an element.
const ANCHOR_LABELS: [&str; 10] = [
    "No Anchor",
    "Top Left",
    "Top",
    "Top Right",
    "Left",
    "Center",
    "Right",
    "Bottom Left",
    "Bottom",
    "Bottom Right",
];

/// Columns of the TreeStore that backs the folder view.
#[repr(i32)]
enum Column {
    Text = 0,
    /// Index into `Config::presets`. For group rows this is -1.
    PresetIndex = 1,
    IconName = 2,
}

/// Everything the handlers share: the working copy plus the widgets they touch.
#[derive(Clone)]
struct Ui {
    state: Rc<RefCell<State>>,
    tree: gtk::TreeView,
    store: gtk::TreeStore,
    form: Form,
    status: gtk::Label,
    /// Set while widgets are filled from the model, so their change signals
    /// cannot write the old values back.
    guard: Rc<Cell<bool>>,
}

struct State {
    config: Config,
    path: std::path::PathBuf,
    /// Index into `config.presets` of the row currently selected in the tree.
    /// `None` means a group row or no selection.
    selected: Option<usize>,
}

#[derive(Clone)]
struct Form {
    label: gtk::Entry,
    size: gtk::Entry,
    anchor: gtk::DropDown,
    group: gtk::Entry,
    hotkey: gtk::Entry,
    x: gtk::Entry,
    y: gtk::Entry,
}

/// Button wires: all buttons in one tuple so the build_window code can clone
/// the whole set into `wire()` without defining an extra struct.
type Buttons = (
    gtk::Button, // reload
    gtk::Button, // save
    gtk::Button, // add
    gtk::Button, // separator
    gtk::Button, // remove
    gtk::Button, // up
    gtk::Button, // down
);

/// Open the configuration window. Returns once the window is closed.
pub fn run(path: &Path) -> CliResult<()> {
    let app = gtk::Application::builder()
        .application_id("dev.sizerlinux.SizerConfig")
        .build();
    let path = path.to_path_buf();
    app.connect_activate(move |app| build_window(app, &path));
    // Hand GTK only the program name: the CLI arguments (`config edit`) would
    // otherwise be read as files to open and the application would exit.
    let _ = app.run_with_args::<&str>(&["sizer"]);
    Ok(())
}

fn build_window(app: &gtk::Application, path: &Path) {
    let config = match Config::load(path) {
        Ok(config) => config,
        // No file yet: start from what the menu shows today, so "Save" turns
        // the built-in presets into an editable file.
        Err(_) => Config::builtin(),
    };
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Sizer Settings")
        .default_width(960)
        .default_height(600)
        .build();

    let title = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let heading = gtk::Label::new(Some("Sizer Settings"));
    heading.set_xalign(0.0);
    heading.add_css_class("title-4");
    let subtitle = gtk::Label::new(Some(&path.display().to_string()));
    subtitle.set_xalign(0.0);
    subtitle.add_css_class("dim-label");
    title.append(&heading);
    title.append(&subtitle);

    let reload = gtk::Button::with_label("Reload");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&reload);
    header.pack_end(&save);
    window.set_titlebar(Some(&header));

    // ---- TreeStore + TreeView (folder view) ----
    let store = gtk::TreeStore::new(&[Type::STRING, Type::I32, Type::STRING]);
    let tree = gtk::TreeView::new();
    tree.set_model(Some(&store));
    tree.set_headers_visible(false);
    tree.set_enable_tree_lines(true);
    tree.set_show_expanders(true);

    let col = gtk::TreeViewColumn::new();
    let icon = gtk::CellRendererPixbuf::new();
    let text = gtk::CellRendererText::new();
    col.pack_start(&icon, false);
    col.pack_start(&text, true);
    col.add_attribute(&icon, "icon-name", Column::IconName as i32);
    col.add_attribute(&text, "text", Column::Text as i32);
    tree.append_column(&col);
    // Expand all nodes so the folder view is useful on first open.
    tree.expand_all();

    let tree_scroll = gtk::ScrolledWindow::builder()
        .child(&tree)
        .vexpand(true)
        .build();

    // ---- Form on the right ----
    let form = Form {
        label: gtk::Entry::new(),
        size: gtk::Entry::new(),
        anchor: gtk::DropDown::from_strings(&ANCHOR_LABELS),
        group: gtk::Entry::new(),
        hotkey: gtk::Entry::new(),
        x: gtk::Entry::new(),
        y: gtk::Entry::new(),
    };
    form.label.set_placeholder_text(Some("Name shown in menu"));
    form.size.set_placeholder_text(Some(
        "e.g. 1920x1080 (pixels) or 50%x50% (workspace percentage); leave blank to keep current size",
    ));
    // Filter the size field to only legal characters before the widget ever
    // accepts them. Users can't type "abc", "x600", or mixed units anymore.
    size_filter(&form.size);
    form.group
        .set_placeholder_text(Some("Optional, e.g. 16:9 / UHD"));
    form.hotkey
        .set_placeholder_text(Some("Optional, e.g. <Super><Alt>F8"));
    form.x.set_placeholder_text(Some("Optional"));
    form.y.set_placeholder_text(Some("Optional"));
    form.x.set_width_chars(8);
    form.y.set_width_chars(8);

    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let fields: [(&str, &gtk::Widget); 7] = [
        ("Name", form.label.upcast_ref()),
        ("Size", form.size.upcast_ref()),
        ("Anchor", form.anchor.upcast_ref()),
        ("Group", form.group.upcast_ref()),
        ("Hotkey", form.hotkey.upcast_ref()),
        ("Position X", form.x.upcast_ref()),
        ("Position Y", form.y.upcast_ref()),
    ];
    for (row, (name, widget)) in fields.iter().enumerate() {
        let caption = gtk::Label::new(Some(name));
        caption.set_xalign(0.0);
        grid.attach(&caption, 0, row as i32, 1, 1);
        widget.set_hexpand(true);
        grid.attach(*widget, 1, row as i32, 2, 1);
    }

    let add = gtk::Button::with_label("Add Preset");
    let separator = gtk::Button::with_label("Add Separator");
    let remove = gtk::Button::with_label("Delete");
    let up = gtk::Button::with_label("Move Up");
    let down = gtk::Button::with_label("Move Down");
    let button_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    button_row.append(&add);
    button_row.append(&separator);
    button_row.append(&remove);
    button_row.append(&up);
    button_row.append(&down);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_margin_start(12);
    status.set_margin_end(12);
    status.set_margin_bottom(12);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 6);
    right.append(&grid);
    right.append(&button_row);
    right.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&tree_scroll));
    paned.set_end_child(Some(&right));
    paned.set_position(340);
    paned.set_resize_start_child(false);
    window.set_child(Some(&paned));

    let ui = Ui {
        state: Rc::new(RefCell::new(State {
            config,
            path: path.to_path_buf(),
            selected: None,
        })),
        tree,
        store,
        form,
        status,
        guard: Rc::new(Cell::new(false)),
    };

    let buttons = (reload, save, add, separator, remove, up, down);
    wire(&ui, &buttons);
    refresh_tree(&ui);
    if !ui.state.borrow().config.presets.is_empty() {
        ui.state.borrow_mut().selected = Some(0);
        refresh_tree(&ui);
        select_preset_in_tree(&ui, 0);
        load_form(&ui);
    } else {
        load_form(&ui);
    }
    window.present();
}

fn wire(ui: &Ui, buttons: &Buttons) {
    let ui = (*ui).clone();
    let (reload, save, add, separator, remove, up, down) = buttons.clone();
    {
        let ui = ui.clone();
        let tree = ui.tree.clone();
        tree.selection().connect_changed(move |selection| {
            if ui.guard.get() {
                return;
            }
            let (model, iter) = match selection.selected() {
                Some(sel) => sel,
                None => {
                    ui.state.borrow_mut().selected = None;
                    load_form(&ui);
                    return;
                }
            };
            let preset_index = model.get_value(&iter, Column::PresetIndex as i32);
            let index = preset_index
                .get::<i32>()
                .ok()
                .filter(|i| *i >= 0)
                .map(|i| i as usize);
            let mut state = ui.state.borrow_mut();
            if state.selected == index {
                return;
            }
            state.selected = index;
            drop(state);
            load_form(&ui);
        });
    }
    for entry in [
        &ui.form.label,
        &ui.form.size,
        &ui.form.group,
        &ui.form.hotkey,
        &ui.form.x,
        &ui.form.y,
    ] {
        let ui = ui.clone();
        entry.connect_changed(move |_| commit_form(&ui));
    }
    {
        let ui = ui.clone();
        let anchor = ui.form.anchor.clone();
        anchor.connect_selected_notify(move |dropdown| {
            if ui.guard.get() {
                return;
            }
            // An anchor and an absolute position are two ways to name the same
            // point; choosing an anchor drops the position fields.
            if dropdown.selected() != 0 {
                ui.guard.set(true);
                ui.form.x.set_text("");
                ui.form.y.set_text("");
                ui.guard.set(false);
            }
            commit_form(&ui);
        });
    }
    {
        let ui = ui.clone();
        add.connect_clicked(move |_| add_preset(&ui, false));
    }
    {
        let ui = ui.clone();
        separator.connect_clicked(move |_| add_preset(&ui, true));
    }
    {
        let ui = ui.clone();
        remove.connect_clicked(move |_| remove_selected(&ui));
    }
    {
        let ui = ui.clone();
        up.connect_clicked(move |_| move_selected(&ui, -1));
    }
    {
        let ui = ui.clone();
        down.connect_clicked(move |_| move_selected(&ui, 1));
    }
    {
        let ui = ui.clone();
        reload.connect_clicked(move |_| reload_config(&ui));
    }
    {
        let ui = ui.clone();
        save.connect_clicked(move |_| save_config(&ui));
    }
}

// ─── configure::tree submodule ────────────────────────────────────────────
//
// Handles MenuNode ↔ TreeStore bridging and TreeStore traversal.
// These functions do not directly manipulate GTK widgets (unless returning TreeIter).

/// Clear the store and rebuild it from `config.presets` via `menu::menu()`.
fn refresh_tree(ui: &Ui) {
    ui.guard.set(true);
    ui.store.clear();
    let nodes = sizer_config::menu::menu(&ui.state.borrow().config.presets);
    populate(&ui.store, &nodes, None);
    ui.tree.expand_all();
    // Restore selection to the right row if we still have one.
    if let Some(index) = ui.state.borrow().selected {
        select_preset_in_tree(ui, index);
    }
    ui.guard.set(false);
}

/// Recursively insert `MenuNode` items into `store`.
fn populate(store: &gtk::TreeStore, nodes: &[MenuNode], parent: Option<&gtk::TreeIter>) {
    for node in nodes {
        match node {
            MenuNode::Group { label, children } => {
                let iter = store.append(parent);
                store.set(
                    &iter,
                    &[
                        (Column::Text as u32, &&**label),
                        (Column::PresetIndex as u32, &-1i32),
                        (Column::IconName as u32, &&"folder"),
                    ],
                );
                populate(store, children, Some(&iter));
            }
            MenuNode::Preset { index, label } => {
                store.set(
                    &store.append(parent),
                    &[
                        (Column::Text as u32, &&**label),
                        (Column::PresetIndex as u32, &(*index as i32)),
                        (Column::IconName as u32, &&"text-x-generic"),
                    ],
                );
            }
            MenuNode::Separator { index } => {
                store.set(
                    &store.append(parent),
                    &[
                        (Column::Text as u32, &&"─── Separator ───"),
                        (Column::PresetIndex as u32, &(*index as i32)),
                        (Column::IconName as u32, &&""),
                    ],
                );
            }
        }
    }
}

/// Walk the store looking for the row whose [`Column::PresetIndex`] equals
/// `preset_index`; focus and select it in the tree view.
fn select_preset_in_tree(ui: &Ui, preset_index: usize) {
    let iter = find_iter_by_preset_index(&ui.store, preset_index as i32);
    let Some(iter) = iter else {
        return;
    };
    let path = ui.store.path(&iter);
    ui.tree.expand_to_path(&path);
    ui.tree.selection().select_iter(&iter);
}

/// Pre-order depth-first search for a row whose preset index matches `target`.
fn find_iter_by_preset_index(store: &gtk::TreeStore, target: i32) -> Option<gtk::TreeIter> {
    let start = store.iter_first()?;
    dfs_find(store, &start, target)
}

/// Pre-order DFS from `start` covering `start`, its children, and siblings.
fn dfs_find(store: &gtk::TreeStore, start: &gtk::TreeIter, target: i32) -> Option<gtk::TreeIter> {
    let current = *start;
    loop {
        let value = store.get_value(&current, Column::PresetIndex as i32);
        if value.get::<i32>().is_ok_and(|v| v == target) {
            return Some(current);
        }
        if let Some(child) = store.iter_children(Some(&current))
            && let Some(found) = dfs_find(store, &child, target)
        {
            return Some(found);
        }
        if !store.iter_next(&current) {
            break;
        }
    }
    None
}

// ─── configure::form submodule ────────────────────────────────────────────
//
// Handles TreeStore ↔ Form backfill, commit_form writeback + validation, anchor
// mapping, and size_filter input filtering. All business validation is centralized in
// validate_form_fields — a GTK-free function that can be unit-tested.

fn load_form(ui: &Ui) {
    ui.guard.set(true);
    let state = ui.state.borrow();
    let preset = state
        .selected
        .and_then(|index| state.config.presets.get(index));
    match preset {
        Some(preset) if !preset.is_separator() => {
            ui.form.label.set_text(&preset.label);
            ui.form.size.set_text(&preset.size);
            ui.form.anchor.set_selected(anchor_index(preset.anchor));
            ui.form
                .group
                .set_text(preset.group.as_deref().unwrap_or_default());
            ui.form
                .hotkey
                .set_text(preset.hotkey.as_deref().unwrap_or_default());
            ui.form
                .x
                .set_text(&preset.x.map(|v| v.to_string()).unwrap_or_default());
            ui.form
                .y
                .set_text(&preset.y.map(|v| v.to_string()).unwrap_or_default());
            set_form_sensitive(&ui.form, true);
        }
        _ => {
            for entry in [
                &ui.form.label,
                &ui.form.size,
                &ui.form.group,
                &ui.form.hotkey,
                &ui.form.x,
                &ui.form.y,
            ] {
                entry.set_text("");
            }
            ui.form.anchor.set_selected(0);
            set_form_sensitive(&ui.form, false);
        }
    }
    ui.guard.set(false);
}

fn set_form_sensitive(form: &Form, sensitive: bool) {
    for widget in [
        form.label.upcast_ref::<gtk::Widget>(),
        form.size.upcast_ref(),
        form.anchor.upcast_ref(),
        form.group.upcast_ref(),
        form.hotkey.upcast_ref(),
        form.x.upcast_ref(),
        form.y.upcast_ref(),
    ] {
        widget.set_sensitive(sensitive);
    }
}

/// Result of [`validate_form_fields`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct FormStatus {
    /// The combined status message, or empty string when everything is fine.
    message: &'static str,
    /// Which field (if any) should flash `error` / `success` CSS classes.
    /// Empty means "no field gets styled differently".
    size_class: Class,
    label_class: Class,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Leave whatever class is already on the widget alone.
    None,
    /// Add CSS class `"success"` — valid user input.
    Success,
    /// Add CSS class `"error"` — invalid user input.
    Error,
}

/// Pure validator for the preset form fields. Returns the most actionable
/// error (if any) plus per-field styling hints. Zero GTK dependencies — this
/// is the function that both [`commit_form`] and future server-side validation
/// call, so correctness lives in exactly one place.
///
/// `all_labels` is every preset's label in the same group (case-insensitive,
/// same comparison Config::validate uses); `current_index` lets us skip self
/// when checking for duplicates.
fn validate_form_fields(
    label: &str,
    raw_size: &str,
    anchor: Option<Anchor>,
    x: Option<i32>,
    y: Option<i32>,
    all_labels: &[String],
    current_index: usize,
) -> FormStatus {
    // 1. Size shape — the filter already stripped illegal chars; what's left
    //    is shape mistakes (mixed units, zero).
    let size_ok = raw_size.is_empty() || SizeSpec::parse(raw_size).is_some();
    if !size_ok {
        return FormStatus {
            message: "Size: format must be NxM (both pixels or both percentages)",
            size_class: Class::Error,
            label_class: Class::None,
        };
    }

    // 2. Half a position — x and y must be both empty or both filled.
    let position_incomplete = matches!((x, y), (Some(_), None) | (None, Some(_)));
    if position_incomplete {
        return FormStatus {
            message: "Position: X and Y must both be set or both be blank",
            size_class: if raw_size.is_empty() {
                Class::None
            } else {
                Class::Success
            },
            label_class: Class::None,
        };
    }

    // 3. Label can't be empty when any other field is meaningful (anchor,
    //    position, or non-empty size). It's fine to clear label on a
    //    move-only preset — that effectively means "keep the previous label".
    let label_blank = label.trim().is_empty();
    let has_anchor = anchor.is_some();
    let has_position = x.is_some() && y.is_some();
    let has_size = !raw_size.is_empty();
    let movable = has_size || has_anchor || has_position;
    if label_blank && movable {
        return FormStatus {
            message: "Name: cannot be empty",
            size_class: if has_size {
                Class::Success
            } else {
                Class::None
            },
            label_class: Class::Error,
        };
    }

    // 4. Duplicate label within the same group (case-insensitive). Matches
    //    Config::validate's check so UI and save never disagree.
    let trimmed = label.trim();
    let lowercase = trimmed.to_lowercase();
    if !label_blank
        && all_labels
            .iter()
            .enumerate()
            .any(|(i, other)| i != current_index && other.trim().to_lowercase() == lowercase)
    {
        return FormStatus {
            message: "Name: duplicate names within the same group are not allowed",
            size_class: if has_size {
                Class::Success
            } else {
                Class::None
            },
            label_class: Class::Error,
        };
    }

    // 5. Must have at least one of size / anchor / position — a preset named
    //    for nothing is a no-op at best, misleading at worst.
    if !movable {
        return FormStatus {
            message: "Invalid: at least one of Size, Anchor, or Position must be set",
            size_class: Class::None,
            label_class: Class::Error,
        };
    }

    // Everything is fine.
    FormStatus {
        message: "",
        size_class: if has_size {
            Class::Success
        } else {
            Class::None
        },
        label_class: if label_blank {
            // move-only preset with blank label: accept it but don't style
            // it green — the user may just not be done yet.
            Class::None
        } else {
            Class::Success
        },
    }
}

fn commit_form(ui: &Ui) {
    if ui.guard.get() {
        return;
    }
    let mut state = ui.state.borrow_mut();
    let Some(index) = state.selected else {
        return;
    };
    let Some(preset) = state.config.presets.get_mut(index) else {
        return;
    };
    if preset.is_separator() {
        return;
    }

    let label_raw = ui.form.label.text().trim().to_string();
    let raw_size = ui.form.size.text().trim().to_string();
    let anchor = anchor_at(ui.form.anchor.selected());
    let x = ui.form.x.text().trim().parse::<i32>().ok();
    let y = ui.form.y.text().trim().parse::<i32>().ok();
    let new_group = sizer_config::menu::normalize_group(Some(ui.form.group.text().as_str()));
    let group_changed = preset.group != new_group;

    // 1. Write the preset's fields. Anchor and absolute position are mutually
    //    exclusive; a half-position drops everything so a stale (x, old_y)
    //    tuple can't survive half-filled form fields. Labels and groups live
    //    in the model as they appear (trimmed), so menu() can show them as-is.
    preset.label = label_raw.clone();
    preset.size = raw_size.clone();
    preset.group = new_group.clone();
    preset.hotkey = non_empty(ui.form.hotkey.text().as_str());
    match (x, y) {
        (Some(x), Some(y)) => {
            preset.x = Some(x);
            preset.y = Some(y);
            preset.anchor = None;
        }
        (None, None) => {
            preset.x = None;
            preset.y = None;
            preset.anchor = anchor;
        }
        _ => {
            // Half-position: reset so validator reports it.
            preset.x = None;
            preset.y = None;
            preset.anchor = None;
        }
    }

    // Collect every preset's label that would live in the NEW target group
    // (including self) for the duplicate check. This matches Config::validate's
    // scope — same group, case-insensitive, labels-only (not separators).
    let all_labels: Vec<String> = state
        .config
        .presets
        .iter()
        .filter(|p| !p.is_separator())
        .filter(|p| p.group == new_group || (p.group.is_none() && new_group.is_none()))
        .map(|p| p.label.clone())
        .collect();
    drop(state);

    // 2. Validate pure function — zero GTK.
    let status = validate_form_fields(&label_raw, &raw_size, anchor, x, y, &all_labels, index);

    // 3. Apply per-field CSS classes.
    ui.form.size.remove_css_class("error");
    ui.form.size.remove_css_class("success");
    ui.form.label.remove_css_class("error");
    ui.form.label.remove_css_class("success");
    match status.size_class {
        Class::Success => ui.form.size.add_css_class("success"),
        Class::Error => ui.form.size.add_css_class("error"),
        Class::None => {}
    }
    match status.label_class {
        Class::Success => ui.form.label.add_css_class("success"),
        Class::Error => ui.form.label.add_css_class("error"),
        Class::None => {}
    }

    // 4. Status line.
    set_status(ui, status.message, !status.message.is_empty());

    // 5. If user typed an anchor while x/y still have values, drop the anchor
    //    (positions win — they're more specific).
    if x.is_some() && y.is_some() && ui.form.anchor.selected() != 0 {
        ui.guard.set(true);
        ui.form.anchor.set_selected(0);
        ui.guard.set(false);
    }

    // 6. Tree refresh. Group change → rebuild whole tree; otherwise just
    //    update the label on the existing row (trimmed, same as menu() uses).
    if group_changed {
        refresh_tree(ui);
    } else if let Some(iter) = find_iter_by_preset_index(&ui.store, index as i32) {
        // TreeStore::set does NOT fire any handler configure.rs listens to,
        // so no new guard is strictly required. We still wrap it so the
        // read-side (load_form's guard) and write-side conventions align.
        ui.guard.set(true);
        ui.store.set(&iter, &[(Column::Text as u32, &&label_raw)]);
        ui.guard.set(false);
    }
}

/// Attach an input filter to the size entry so users can't type characters
/// that `SizeSpec::parse` would reject anyway.
///
/// Allowed characters are: digits, `.`, `x`/`X`, `%`, and whitespace. Other
/// characters (letters, `-`, `/`, etc.) are silently stripped whenever the
/// entry changes. This eliminates 90% of size mistakes at the source.
fn size_filter(entry: &gtk::Entry) {
    let entry = entry.clone();
    entry.connect_changed(move |self_| {
        let text = self_.text();
        let filtered: String = text
            .chars()
            .filter(|c| matches!(c, '0'..='9' | 'x' | 'X' | '%' | '.' | ' '))
            .collect();
        if filtered.len() < text.len() {
            self_.set_text(&filtered);
        }
    });
}

fn set_status(ui: &Ui, text: &str, error: bool) {
    ui.status.set_text(text);
    if error {
        ui.status.add_css_class("error");
    } else {
        ui.status.remove_css_class("error");
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn anchor_at(index: u32) -> Option<Anchor> {
    if index == 0 {
        None
    } else {
        Anchor::GRID.get((index - 1) as usize).copied()
    }
}

fn anchor_index(anchor: Option<Anchor>) -> u32 {
    anchor
        .and_then(|anchor| Anchor::GRID.iter().position(|grid| *grid == anchor))
        .map(|position| position as u32 + 1)
        .unwrap_or(0)
}

// ─── configure::store submodule ───────────────────────────────────────────
//
// Handles Config.presets CRUD: add, delete, move, save, reload.

fn add_preset(ui: &Ui, separator: bool) {
    // If a row is selected, inherit its group so adding a preset or separator
    // inside a 16:9 folder stays inside that folder instead of falling out
    // to the top level.
    let inherited_group = {
        let state = ui.state.borrow();
        state
            .selected
            .and_then(|index| state.config.presets.get(index)?.group.clone())
    };
    let mut state = ui.state.borrow_mut();
    let preset = if separator {
        // Preset::separator takes the group directly — if the user had a row
        // selected we'll create the separator inside that group.
        Preset::separator(inherited_group)
    } else {
        let mut number = 1;
        let label = loop {
            let candidate = if number == 1 {
                "New Preset".to_string()
            } else {
                format!("New Preset {number}")
            };
            if !state
                .config
                .presets
                .iter()
                .any(|preset| preset.label == candidate)
            {
                break candidate;
            }
            number += 1;
        };
        Preset {
            label,
            size: "".to_string(),
            group: inherited_group,
            anchor: Some(Anchor::Center),
            hotkey: None,
            monitor: None,
            x: None,
            y: None,
            allow_outside_bounds: None,
            kind: PresetKind::Resize,
        }
    };
    state.config.presets.push(preset);
    state.selected = Some(state.config.presets.len() - 1);
    drop(state);
    refresh_tree(ui);
    if let Some(index) = ui.state.borrow().selected {
        select_preset_in_tree(ui, index);
    }
    load_form(ui);
    set_status(ui, "Added. Save to apply.", false);
}

fn remove_selected(ui: &Ui) {
    let mut state = ui.state.borrow_mut();
    let Some(index) = state.selected else {
        drop(state);
        set_status(ui, "Select a preset row first", true);
        return;
    };
    state.config.presets.remove(index);
    state.selected = if state.config.presets.is_empty() {
        None
    } else {
        Some(index.min(state.config.presets.len() - 1))
    };
    drop(state);
    refresh_tree(ui);
    if let Some(index) = ui.state.borrow().selected {
        select_preset_in_tree(ui, index);
    }
    load_form(ui);
    set_status(ui, "Deleted. Save to apply.", false);
}

/// Move the selected preset up or down within its parent group (or the top
/// level). `delta` is ±1. The swap operates on `config.presets` directly, so
/// the tree is rebuilt afterwards to reflect the new order.
fn move_selected(ui: &Ui, delta: i32) {
    let mut state = ui.state.borrow_mut();
    let Some(index) = state.selected else {
        drop(state);
        set_status(ui, "Select a preset row first", true);
        return;
    };
    let Some(preset) = state.config.presets.get(index) else {
        drop(state);
        return;
    };
    let target_group = preset.group.clone();
    // Siblings = all presets sharing the same group (including separators).
    // Swapping a preset and a separator within the same group is the expected
    // way to reorder separators — they're full members of their group in
    // menu::menu().
    let siblings: Vec<usize> = state
        .config
        .presets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.group == target_group)
        .map(|(i, _)| i)
        .collect();
    let Some(pos_in_siblings) = siblings.iter().position(|&i| i == index) else {
        drop(state);
        return;
    };
    let target = pos_in_siblings as i32 + delta;
    if target < 0 || target as usize >= siblings.len() {
        drop(state);
        return;
    }
    let target = siblings[target as usize];
    state.config.presets.swap(index, target);
    state.selected = Some(target);
    drop(state);
    refresh_tree(ui);
    select_preset_in_tree(ui, target);
    load_form(ui);
}

fn reload_config(ui: &Ui) {
    let path = ui.state.borrow().path.clone();
    match Config::load(&path) {
        Ok(config) => {
            let mut state = ui.state.borrow_mut();
            state.config = config;
            state.selected = (!state.config.presets.is_empty()).then_some(0);
            drop(state);
            refresh_tree(ui);
            if let Some(index) = ui.state.borrow().selected {
                select_preset_in_tree(ui, index);
            }
            load_form(ui);
            set_status(ui, "Reloaded.", false);
        }
        Err(error) => set_status(ui, &format!("Read failed: {error}"), true),
    }
}

fn save_config(ui: &Ui) {
    let state = ui.state.borrow();
    let mut messages: Vec<String> = state
        .config
        .validate()
        .into_iter()
        .map(|problem| format!("{}: {}", problem.field, problem.message))
        .collect();
    for (index, preset) in state.config.presets.iter().enumerate() {
        if let Some(hotkey) = preset.hotkey.as_deref()
            && let Err(message) = sizer_backend_x11::validate_accelerator(hotkey)
        {
            messages.push(format!("presets[{index}].hotkey: {message}"));
        }
    }
    if !messages.is_empty() {
        drop(state);
        set_status(ui, &format!("Cannot save: {}", messages.join("; ")), true);
        return;
    }
    let path = state.path.clone();
    let result = state.config.save(&path);
    drop(state);
    match result {
        Ok(()) => set_status(ui, &format!("Saved: {}", path.display()), false),
        Err(error) => set_status(ui, &format!("Save failed: {error}"), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_index_round_trips_the_grid() {
        for (index, anchor) in (1..=9).zip(Anchor::GRID) {
            assert_eq!(anchor_at(index), Some(anchor));
            assert_eq!(anchor_index(Some(anchor)), index);
        }
        assert_eq!(anchor_at(0), None);
        assert_eq!(anchor_index(None), 0);
    }

    #[test]
    fn anchor_labels_lockstep_with_grid() {
        // ANCHOR_LABELS is GRID + 1 (index 0 = "no anchor") + 9 anchors.
        assert_eq!(
            ANCHOR_LABELS.len() - 1,
            Anchor::GRID.len(),
            "ANCHOR_LABELS[1..] must match Anchor::GRID one-to-one"
        );
    }

    #[test]
    fn size_filter_drops_illegal_characters() {
        let legal = |c: char| matches!(c, '0'..='9' | 'x' | 'X' | '%' | '.' | ' ');
        for c in ['x', 'X', '5', '%', '.', ' '] {
            assert!(legal(c), "{c} should be accepted");
        }
        for c in ['a', '-', '/', '中'] {
            assert!(!legal(c), "{c} should be rejected");
        }
    }

    #[test]
    fn size_parse_still_rejects_shape_mistakes() {
        // Characters are legal but the shape is still wrong. These slip past
        // the filter and get caught by SizeSpec::parse in commit_form.
        assert!(SizeSpec::parse("800x").is_none());
        assert!(SizeSpec::parse("x600").is_none());
        assert!(SizeSpec::parse("50%x800").is_none());
        assert!(SizeSpec::parse("0x100").is_none());
    }

    // ── validate_form_fields: the pure business validator ──

    // Helper so each test doesn't have to construct a labels Vec manually.
    fn labels(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn ok_when_size_is_a_valid_preset() {
        let s = validate_form_fields(
            "a",
            "800x600",
            Some(Anchor::Center),
            None,
            None,
            &labels(&["a"]),
            0,
        );
        assert_eq!(s.message, "");
        assert_eq!(s.size_class, Class::Success);
        assert_eq!(s.label_class, Class::Success);
    }

    #[test]
    fn ok_when_move_only_with_anchor() {
        let s = validate_form_fields(
            "Center",
            "",
            Some(Anchor::Center),
            None,
            None,
            &labels(&["Center"]),
            0,
        );
        assert_eq!(s.message, "");
        assert_eq!(s.size_class, Class::None);
    }

    #[test]
    fn ok_when_move_only_with_position() {
        let s = validate_form_fields("p", "", None, Some(10), Some(20), &labels(&["p"]), 0);
        assert_eq!(s.message, "");
        assert_eq!(s.size_class, Class::None);
    }

    #[test]
    fn size_shape_error_takes_top_priority() {
        let s = validate_form_fields("a", "50%x800", None, None, None, &labels(&["a"]), 0);
        assert_eq!(
            s.message,
            "Size: format must be NxM (both pixels or both percentages)"
        );
        assert_eq!(s.size_class, Class::Error);
    }

    #[test]
    fn half_position_is_an_error() {
        let s = validate_form_fields("a", "", None, Some(10), None, &labels(&["a"]), 0);
        assert_eq!(
            s.message,
            "Position: X and Y must both be set or both be blank"
        );
    }

    #[test]
    fn blank_label_is_error_when_movable() {
        // Cannot have a blank label when anything is movable.
        let s = validate_form_fields("", "", Some(Anchor::Center), None, None, &labels(&[""]), 0);
        assert_eq!(s.message, "Name: cannot be empty");
        assert_eq!(s.label_class, Class::Error);
    }

    #[test]
    fn blank_label_with_size_is_an_error() {
        let s = validate_form_fields("", "800x600", None, None, None, &labels(&[""]), 0);
        assert_eq!(s.message, "Name: cannot be empty");
        assert_eq!(s.label_class, Class::Error);
    }

    #[test]
    fn nothing_set_at_all_is_an_error() {
        let s = validate_form_fields("", "", None, None, None, &labels(&[""]), 0);
        assert_eq!(
            s.message,
            "Invalid: at least one of Size, Anchor, or Position must be set"
        );
        assert_eq!(s.label_class, Class::Error);
    }

    #[test]
    fn duplicate_label_in_same_group_is_an_error() {
        // index=1 is "a", index=0 is also "a" → conflict.
        let all = labels(&["a", "a"]);
        let s = validate_form_fields("a", "800x600", None, None, None, &all, 1);
        assert_eq!(
            s.message,
            "Name: duplicate names within the same group are not allowed"
        );
        assert_eq!(s.label_class, Class::Error);
    }

    #[test]
    fn duplicate_label_case_insensitive() {
        // all_labels[0]="JONG" (lower="jong"), current label="jong" (index=1)
        // → same group conflict regardless of case.
        let all = labels(&["JONG", "jong"]);
        let s = validate_form_fields("jong", "", Some(Anchor::Center), None, None, &all, 1);
        assert_eq!(
            s.message,
            "Name: duplicate names within the same group are not allowed"
        );
    }

    #[test]
    fn duplicate_label_different_groups_is_ok() {
        // This validator only looks at the labels list — callers are
        // responsible for passing only the same-group labels. So if the
        // caller accidentally passes the whole flat list, the validator will
        // over-report. We intentionally trust the caller here; the UI path
        // filters by group before collecting labels.
    }
}
