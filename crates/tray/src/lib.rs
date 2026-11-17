//! StatusNotifierItem tray: one menu entry per configured preset.
//!
//! The tray process owns the D-Bus name `org.kde.StatusNotifierItem-<pid>-1` and
//! applies presets through the same X11 backend as the CLI, so a click and
//! `sizer apply <label>` cannot drift apart.

use ksni::blocking::Handle;
use ksni::blocking::TrayMethods as _;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Icon, Tray};

use std::path::PathBuf;

use sizer_backend_x11::X11Backend;
use sizer_config::Config;
use sizer_config::menu::{self, MenuNode};
use sizer_core::{Anchor, AreaKind, MonitorSelector, Rect, SizeBasis, SizeSpec, WindowId};
use sizer_platform::{WindowBackend, WindowSelector, resolve_selector};

/// One preset the tray can apply.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayPreset {
    pub label: String,
    /// `None` is a move-only preset, which keeps the window's current size.
    pub size: Option<SizeSpec>,
    /// Absolute position inside the area; takes precedence over `anchor`.
    pub position: Option<(i32, i32)>,
    pub anchor: Option<Anchor>,
    pub monitor: MonitorSelector,
    pub allow_outside: bool,
}

/// Everything the tray needs from the configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayConfig {
    /// One slot per configured entry, so menu indices keep pointing at the same
    /// preset. Separators occupy a slot but carry no preset.
    pub presets: Vec<Option<TrayPreset>>,
    /// Preset groups as cascade submenus, in configuration order.
    pub menu: Vec<MenuNode>,
    pub area: AreaKind,
    pub basis: SizeBasis,
    pub allow_outside: bool,
}

impl TrayConfig {
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let mut presets = Vec::with_capacity(config.presets.len());
        for preset in &config.presets {
            if preset.is_separator() {
                presets.push(None);
                continue;
            }
            presets.push(Some(TrayPreset {
                label: preset.label.clone(),
                size: if preset.is_move_only() {
                    None
                } else {
                    Some(preset.size_spec()?)
                },
                position: preset.position(),
                anchor: preset.anchor,
                monitor: preset.monitor_selector()?,
                allow_outside: preset.allows_outside(config.options.allow_outside_bounds),
            }));
        }
        Ok(Self {
            presets,
            menu: menu::menu(&config.presets),
            area: config.options.area,
            basis: config.options.basis,
            allow_outside: config.options.allow_outside_bounds,
        })
    }
}

/// The SNI object. `menu()` is called again after every activation.
pub struct TrayItem {
    config: TrayConfig,
}

impl Tray for TrayItem {
    // `ItemIsMenu` must stay false so the host forwards activation (a double
    // click on Ubuntu's indicator) to `activate`; the host still shows `menu`
    // on a plain click and on right click.
    const MENU_ON_ACTIVATE: bool = false;

    fn id(&self) -> String {
        "sizer".to_string()
    }

    fn title(&self) -> String {
        "Sizer".to_string()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![tray_icon()]
    }

    /// Double click on the icon: open the configuration window.
    fn activate(&mut self, _x: i32, _y: i32) {
        open_config_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // First row names the window the presets would act on, like Sizer's
        // tray menu. It is resolved every time the menu opens so it tracks focus.
        let mut items = vec![MenuItem::Standard(StandardItem {
            label: target_label_for_menu(),
            enabled: false,
            ..Default::default()
        })];
        if self.config.presets.is_empty() {
            items.push(MenuItem::Standard(StandardItem {
                label: "No presets configured".to_string(),
                enabled: false,
                ..Default::default()
            }));
        } else {
            items.extend(menu_items(&self.config.menu));
        }
        items.push(MenuItem::Separator);
        items.push(MenuItem::Standard(StandardItem {
            label: "Settings…".to_string(),
            activate: Box::new(|_: &mut TrayItem| open_config_window()),
            ..Default::default()
        }));
        items
    }
}

/// Open `sizer config edit` as its own process, so a crash cannot take the
/// tray down and the window survives tray restarts.
fn open_config_window() {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sizer"));
    if let Err(error) = std::process::Command::new(exe)
        .args(["config", "edit"])
        .spawn()
    {
        eprintln!("sizer: config edit: {error}");
    }
}

/// Render the shared menu tree as tray items: groups become cascade submenus.
fn menu_items(nodes: &[MenuNode]) -> Vec<MenuItem<TrayItem>> {
    nodes
        .iter()
        .map(|node| match node {
            MenuNode::Preset { index, label } => {
                let index = *index;
                MenuItem::Standard(StandardItem {
                    label: label.clone(),
                    activate: Box::new(move |tray: &mut TrayItem| activate(tray, index)),
                    ..Default::default()
                })
            }
            MenuNode::Separator { .. } => MenuItem::Separator,
            MenuNode::Group { label, children } => MenuItem::SubMenu(SubMenu {
                label: label.clone(),
                submenu: menu_items(children),
                ..Default::default()
            }),
        })
        .collect()
}

fn activate(tray: &mut TrayItem, index: usize) {
    let Some(Some(preset)) = tray.config.presets.get(index).cloned() else {
        return;
    };
    let area = tray.config.area;
    let basis = tray.config.basis;
    // Applying talks to the X server; keep the menu responsive.
    std::thread::spawn(move || match apply_preset(&preset, area, basis) {
        Ok(summary) => println!("{summary}"),
        Err(error) => eprintln!("sizer: {}: {error}", preset.label),
    });
}

/// Register the tray service on the session bus.
pub fn spawn(config: TrayConfig) -> Result<Handle<TrayItem>, String> {
    TrayItem { config }
        .assume_sni_available(true)
        .spawn()
        .map_err(|error| error.to_string())
}

const NO_TARGET: &str = "no active window";

/// Label of the first menu row: the window a preset click would resize.
fn target_label_for_menu() -> String {
    let Ok(backend) = X11Backend::connect() else {
        return NO_TARGET.to_string();
    };
    let Ok(id) = resolve_selector(&backend, &WindowSelector::Active) else {
        return NO_TARGET.to_string();
    };
    let info = backend
        .list_windows()
        .ok()
        .and_then(|windows| windows.into_iter().find(|info| info.id == id));
    match info {
        Some(info) => target_label(info.app_id.as_deref(), &info.title),
        None => NO_TARGET.to_string(),
    }
}

/// `<app>: <title>`, degrading to whichever half is present.
fn target_label(app: Option<&str>, title: &str) -> String {
    let title = title.trim();
    let app = app.map(str::trim).filter(|app| !app.is_empty());
    match (app, title.is_empty()) {
        (Some(app), false) => format!("{app}: {title}"),
        (Some(app), true) => app.to_string(),
        (None, false) => title.to_string(),
        (None, true) => NO_TARGET.to_string(),
    }
}

/// Apply a preset to the active window and describe what happened.
pub fn apply_preset(
    preset: &TrayPreset,
    area_kind: AreaKind,
    basis: SizeBasis,
) -> Result<String, String> {
    let backend = X11Backend::connect().map_err(|error| error.to_string())?;
    let id: WindowId =
        resolve_selector(&backend, &WindowSelector::Active).map_err(|error| error.to_string())?;
    let area = backend
        .area(preset.monitor, area_kind)
        .map_err(|error| error.to_string())?;
    let current = backend
        .geometry(&id)
        .map_err(|error| error.to_string())?
        .rect(basis);
    let size = preset
        .size
        .map(|size| size.resolve(area))
        .unwrap_or((current.w, current.h));
    let origin = match (preset.position, preset.anchor) {
        (Some((x, y)), _) => Rect::new(area.x + x, area.y + y, size.0, size.1),
        (None, Some(anchor)) => anchor.place(area, size),
        (None, None) => current,
    };
    let result = backend
        .apply(
            &id,
            Rect::new(origin.x, origin.y, size.0, size.1),
            basis,
            area,
            preset.allow_outside,
        )
        .map_err(|error| error.to_string())?;
    let (dx, dy, dw, dh) = result.delta();
    Ok(format!(
        "{} applied to {id}: {} {}x{} at {},{} delta {dx},{dy},{dw},{dh}",
        preset.label,
        match result.basis {
            SizeBasis::Client => "client",
            SizeBasis::Frame => "frame",
        },
        result.achieved.w,
        result.achieved.h,
        result.achieved.x,
        result.achieved.y,
    ))
}

/// A 22x22 window glyph in ARGB32 network byte order.
fn tray_icon() -> Icon {
    const SIZE: i32 = 22;
    let mut data = vec![0u8; (SIZE * SIZE * 4) as usize];
    let white = [255, 246, 245, 244];
    let accent = [255, 53, 132, 228];
    for x in 2..20 {
        put(&mut data, SIZE, x, 3, white);
        put(&mut data, SIZE, x, 18, white);
    }
    for y in 3..19 {
        put(&mut data, SIZE, 2, y, white);
        put(&mut data, SIZE, 19, y, white);
    }
    for y in 4..8 {
        for x in 3..19 {
            put(&mut data, SIZE, x, y, accent);
        }
    }
    Icon {
        width: SIZE,
        height: SIZE,
        data,
    }
}

fn put(data: &mut [u8], size: i32, x: i32, y: i32, color: [u8; 4]) {
    if !(0..size).contains(&x) || !(0..size).contains(&y) {
        return;
    }
    let offset = ((y * size + x) * 4) as usize;
    data[offset..offset + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version = 1

[options]
area = "monitor"
basis = "frame"
tray = true

[[presets]]
label = "1080p"
size = "1920x1080"
group = "16:9"
anchor = "center"

[[presets]]
label = "half"
size = "50%x50%"
monitor = "primary"

[[presets]]
label = "pinned"
size = ""
x = 120
y = 64
monitor = "1"
allow_outside_bounds = true

[[presets]]
label = "sized"
size = "300x200"
x = 200
y = 90
"#;

    #[test]
    fn builds_presets_from_config() {
        let config = Config::from_toml(SAMPLE).expect("sample");
        let tray = TrayConfig::from_config(&config).expect("tray config");
        assert_eq!(tray.area, AreaKind::Monitor);
        assert_eq!(tray.basis, SizeBasis::Frame);
        assert_eq!(tray.presets.len(), 4);
        // Every row is `Option`: `None` is a separator, so the index matches the
        // menu item the user clicks.
        let first = tray.presets[0].as_ref().expect("a preset row");
        assert_eq!(first.label, "1080p");
        assert_eq!(first.size, Some(SizeSpec::Pixels(1920, 1080)));
        assert_eq!(first.anchor, Some(Anchor::Center));
        assert_eq!(first.position, None);
        let second = tray.presets[1].as_ref().expect("a preset row");
        assert_eq!(second.monitor, MonitorSelector::Primary);
        assert_eq!(second.size, Some(SizeSpec::Percent(50.0, 50.0)));
        // The tray carries an absolute position, so a coordinate preset applies
        // the same place from the tray as from `sizer apply`.
        let pinned = tray.presets[2].as_ref().expect("a positioned preset");
        assert_eq!(pinned.position, Some((120, 64)));
        assert_eq!(pinned.anchor, None);
        assert_eq!(pinned.monitor, MonitorSelector::Index(1));
        // An empty size is the move-only form; the switch is per preset.
        assert_eq!(pinned.size, None);
        assert!(pinned.allow_outside);
        let sized = tray.presets[3].as_ref().expect("a sized position preset");
        assert_eq!(sized.size, Some(SizeSpec::Pixels(300, 200)));
        assert!(!sized.allow_outside);
    }

    #[test]
    fn groups_become_cascade_submenus() {
        let config = Config::from_toml(SAMPLE).expect("sample");
        let tray = TrayConfig::from_config(&config).expect("tray config");
        let items = menu_items(&tray.menu);
        assert_eq!(items.len(), 4);
        match &items[0] {
            MenuItem::SubMenu(group) => {
                assert_eq!(group.label, "16:9");
                assert_eq!(group.submenu.len(), 1);
                match &group.submenu[0] {
                    MenuItem::Standard(item) => assert_eq!(item.label, "1080p"),
                    _ => panic!("expected a preset row"),
                }
            }
            _ => panic!("expected a group"),
        }
        match &items[1] {
            MenuItem::Standard(item) => {
                assert_eq!(item.label, "half");
                assert!(item.enabled);
            }
            _ => panic!("expected an ungrouped preset"),
        }
        match &items[2] {
            MenuItem::Standard(item) => assert_eq!(item.label, "pinned"),
            _ => panic!("expected an ungrouped preset"),
        }
        match &items[3] {
            MenuItem::Standard(item) => assert_eq!(item.label, "sized"),
            _ => panic!("expected an ungrouped preset"),
        }
    }

    #[test]
    fn rejects_broken_presets() {
        let config =
            Config::from_toml("version = 1\n[[presets]]\nlabel = \"x\"\nsize = \"wide\"\n")
                .expect("toml");
        assert!(TrayConfig::from_config(&config).is_err());
    }

    #[test]
    fn target_label_names_the_window() {
        assert_eq!(
            target_label(Some("org.gnome.Nautilus"), "Home"),
            "org.gnome.Nautilus: Home"
        );
        assert_eq!(target_label(None, "Home"), "Home");
        assert_eq!(target_label(Some("firefox"), "  "), "firefox");
        assert_eq!(target_label(Some("  "), "Home"), "Home");
        assert_eq!(target_label(None, " "), NO_TARGET);
    }

    #[test]
    fn icon_is_argb32_and_opaque() {
        let icon = tray_icon();
        assert_eq!(icon.width, 22);
        assert_eq!(icon.height, 22);
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        // The title bar must be fully opaque.
        let offset = ((5 * 22 + 5) * 4) as usize;
        assert_eq!(icon.data[offset], 255);
    }
}
