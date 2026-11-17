//! Versioned TOML configuration.
//!
//! The native file is authoritative. Sizer's `sizer.xml` is only imported and
//! exported, never read at runtime, so Win32-isms (virtual key codes, DWM frame
//! compensation) never leak into the model.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sizer_core::{Anchor, AreaKind, MonitorSelector, Rect, SizeBasis, SizeSpec};

pub mod menu;
pub mod sizer_xml;

pub const CURRENT_VERSION: u32 = 1;

/// Accelerator that opens the preset menu when no configuration file exists.
/// Kept in step with the GNOME schema default and with Sizer's `Ctrl+Win+Z`.
pub const BUILTIN_HOTKEY: &str = "<Primary><Super>z";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            options: Options::default(),
            presets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub area: AreaKind,
    pub basis: SizeBasis,
    pub tray: bool,
    /// Position presets may leave the reference area. Off by default, so they
    /// behave the same whatever the window manager would allow.
    #[serde(default)]
    pub allow_outside_bounds: bool,
    /// Accelerator that opens the menu, e.g. `<Super><Ctrl>z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            area: AreaKind::WorkArea,
            basis: SizeBasis::Client,
            tray: true,
            allow_outside_bounds: false,
            hotkey: None,
        }
    }
}

/// One preset as stored on disk. `size` stays a string so invalid values can be
/// reported with the label that contains them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub label: String,
    /// `1920x1080` or `50%x50%`; empty for a menu separator.
    pub size: String,
    /// Menu section the preset belongs to, nested groups joined with ` / `.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// Absolute position inside the reference area, in the configured basis.
    /// Both are required together and exclude `anchor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Write this window even when it leaves the reference area. `None` follows
    /// `[options] allow_outside_bounds`; Sizer's `<relocate allowOutsideBounds>`
    /// maps here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_outside_bounds: Option<bool>,
    /// A separator draws a line in the menu and cannot be applied.
    #[serde(default, skip_serializing_if = "PresetKind::is_resize")]
    pub kind: PresetKind,
}

/// What an entry in `presets` is. Sizer's `<separator />` shares the menu list
/// with resizable presets, so separators keep their position in the same vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetKind {
    #[default]
    Resize,
    Separator,
}

impl PresetKind {
    pub fn is_resize(&self) -> bool {
        matches!(self, PresetKind::Resize)
    }
}

impl Preset {
    pub fn separator(group: Option<String>) -> Self {
        Self {
            label: String::new(),
            size: String::new(),
            group,
            anchor: None,
            hotkey: None,
            monitor: None,
            x: None,
            y: None,
            allow_outside_bounds: None,
            kind: PresetKind::Separator,
        }
    }

    pub fn is_separator(&self) -> bool {
        self.kind == PresetKind::Separator
    }

    /// `(x, y)` when this preset names an absolute position.
    pub fn position(&self) -> Option<(i32, i32)> {
        match (self.x, self.y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        }
    }

    /// Whether this preset may leave the reference area, given the global switch.
    pub fn allows_outside(&self, default: bool) -> bool {
        self.allow_outside_bounds.unwrap_or(default)
    }

    /// A preset that only moves the window: Sizer's `<relocate>`.
    ///
    /// The window keeps the size it has right now. Either an absolute position
    /// or an anchor tells the preset where to move it.
    pub fn is_move_only(&self) -> bool {
        !self.is_separator()
            && self.size.trim().is_empty()
            && (self.position().is_some() || self.anchor.is_some())
    }

    /// Where this preset puts a window of `size` inside `area`.
    ///
    /// `None` means "keep the current position". Coordinates are relative to
    /// the area origin and are expressed in the basis the caller passes to
    /// `apply` (client or frame), so they are written verbatim.
    pub fn place(&self, area: Rect, size: (i32, i32)) -> Option<Rect> {
        if let Some((x, y)) = self.position() {
            return Some(Rect::new(area.x + x, area.y + y, size.0, size.1));
        }
        self.anchor.map(|anchor| anchor.place(area, size))
    }

    pub fn size_spec(&self) -> Result<SizeSpec, String> {
        SizeSpec::parse(&self.size)
            .ok_or_else(|| format!("size {:?} must look like 1024x768 or 50%x50%", self.size))
    }

    pub fn monitor_selector(&self) -> Result<MonitorSelector, String> {
        match self.monitor.as_deref() {
            None | Some("") | Some("active") => Ok(MonitorSelector::Active),
            Some("primary") => Ok(MonitorSelector::Primary),
            Some(text) => text
                .parse::<usize>()
                .map(MonitorSelector::Index)
                .map_err(|_| format!("monitor {text:?} must be active, primary or an index")),
        }
    }
}

/// One validation failure, pointing at the field that caused it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub field: String,
    pub message: String,
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| ConfigError::IoAt(path.to_path_buf(), error))?;
        Self::from_toml(&text)
    }

    /// Write through a sibling temporary file so an interrupted save never
    /// leaves a half-written configuration behind.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| ConfigError::IoAt(parent.to_path_buf(), error))?;
        }
        let encoded = self.to_toml()?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, encoded)
            .map_err(|error| ConfigError::IoAt(temporary.clone(), error))?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(ConfigError::IoAt(path.to_path_buf(), error));
        }
        Ok(())
    }

    /// `$XDG_CONFIG_HOME/sizer/config.toml`, falling back to `~/.config`.
    pub fn default_path() -> Option<PathBuf> {
        config_path(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// Classic Sizer presets used by `popup`, `hotkeys` and `tray` when no
    /// configuration file has been written yet, so the first run needs no TOML.
    /// Sizes are grouped by aspect ratio, the way Sizer's menu sections them.
    pub fn builtin() -> Self {
        let preset = |label: &str, size: &str, group: Option<&str>| Preset {
            label: label.to_string(),
            size: size.to_string(),
            group: group.map(str::to_string),
            anchor: Some(Anchor::Center),
            hotkey: None,
            monitor: None,
            x: None,
            y: None,
            allow_outside_bounds: None,
            kind: PresetKind::Resize,
        };
        Self {
            version: CURRENT_VERSION,
            options: Options {
                hotkey: Some(BUILTIN_HOTKEY.to_string()),
                ..Options::default()
            },
            presets: vec![
                // The first row keeps the window size and only centres it.
                preset("Center", "", None),
                preset("50% x 50%", "50%x50%", None),
                preset("800x600", "800x600", Some("4:3")),
                preset("1024x768", "1024x768", Some("4:3")),
                preset("1280x960", "1280x960", Some("4:3")),
                preset("1600x1200", "1600x1200", Some("4:3")),
                preset("1280x720", "1280x720", Some("16:9")),
                preset("1600x900", "1600x900", Some("16:9")),
                preset("1920x1080", "1920x1080", Some("16:9")),
                preset("2560x1440", "2560x1440", Some("16:9")),
                preset("1280x800", "1280x800", Some("16:10")),
                preset("1440x900", "1440x900", Some("16:10")),
                preset("1680x1050", "1680x1050", Some("16:10")),
                preset("1920x1200", "1920x1200", Some("16:10")),
            ],
        }
    }

    /// Structural checks the loader cannot express, returned all at once.
    pub fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        if self.version != CURRENT_VERSION {
            problems.push(Problem {
                field: "version".to_string(),
                message: format!(
                    "unsupported version {}, expected {CURRENT_VERSION}",
                    self.version
                ),
            });
        }
        let mut seen: Vec<(String, String)> = Vec::new();
        for (index, preset) in self.presets.iter().enumerate() {
            let field = format!("presets[{index}]");
            let group = preset
                .group
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if preset.is_separator() {
                // A separator is not a preset: only its placement is meaningful.
                if !preset.label.trim().is_empty()
                    || !preset.size.trim().is_empty()
                    || preset.anchor.is_some()
                    || preset.hotkey.is_some()
                    || preset.monitor.is_some()
                    || preset.x.is_some()
                    || preset.y.is_some()
                    || preset.allow_outside_bounds.is_some()
                {
                    problems.push(Problem {
                        field,
                        message: "a separator must not carry a label, size, anchor, position, \
                                  hotkey, monitor or the outside-bounds switch"
                            .to_string(),
                    });
                }
                continue;
            }
            let label = preset.label.trim().to_ascii_lowercase();
            if label.is_empty() {
                problems.push(Problem {
                    field: format!("{field}.label"),
                    message: "label must not be empty".to_string(),
                });
            } else if seen
                .iter()
                .any(|(seen_group, seen_label)| *seen_group == group && *seen_label == label)
            {
                problems.push(Problem {
                    field: format!("{field}.label"),
                    message: format!("duplicate label {:?} in the same group", preset.label),
                });
            }
            seen.push((group, label));
            // An empty size is the move-only form (Sizer's `<relocate>`), which
            // keeps the window's current size; a position or an anchor says
            // where to move it.
            if preset.size.trim().is_empty() {
                if preset.position().is_none() && preset.anchor.is_none() {
                    problems.push(Problem {
                        field: format!("{field}.size"),
                        message: "size is required unless the preset names a position or an anchor"
                            .to_string(),
                    });
                }
            } else if let Err(message) = preset.size_spec() {
                problems.push(Problem {
                    field: format!("{field}.size"),
                    message,
                });
            }
            if let Err(message) = preset.monitor_selector() {
                problems.push(Problem {
                    field: format!("{field}.monitor"),
                    message,
                });
            }
            if preset
                .hotkey
                .as_deref()
                .is_some_and(|key| key.trim().is_empty())
            {
                problems.push(Problem {
                    field: format!("{field}.hotkey"),
                    message: "hotkey must not be empty".to_string(),
                });
            }
            match (preset.x, preset.y) {
                (Some(_), None) | (None, Some(_)) => problems.push(Problem {
                    field: format!("{field}.x"),
                    message: "x and y must be set together".to_string(),
                }),
                (Some(_), Some(_)) if preset.anchor.is_some() => problems.push(Problem {
                    field: format!("{field}.anchor"),
                    message: "anchor and x/y are mutually exclusive".to_string(),
                }),
                _ => {}
            }
            if preset.allow_outside_bounds.is_some() && preset.position().is_none() {
                problems.push(Problem {
                    field: format!("{field}.allow_outside_bounds"),
                    message: "the outside-bounds switch needs x and y".to_string(),
                });
            }
        }
        if self
            .options
            .hotkey
            .as_deref()
            .is_some_and(|key| key.trim().is_empty())
        {
            problems.push(Problem {
                field: "options.hotkey".to_string(),
                message: "hotkey must not be empty".to_string(),
            });
        }
        problems
    }
}

fn config_path(
    config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(directory) = config_home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(directory).join("sizer").join("config.toml"));
    }
    home.filter(|value| !value.is_empty()).map(|directory| {
        PathBuf::from(directory)
            .join(".config")
            .join("sizer")
            .join("config.toml")
    })
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    /// An I/O failure tied to a specific file, so the message names it.
    IoAt(PathBuf, std::io::Error),
    Parse(toml::de::Error),
    Encode(toml::ser::Error),
    Xml(String),
    Invalid(Vec<Problem>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(error) => write!(formatter, "config io error: {error}"),
            ConfigError::IoAt(path, error) => {
                write!(formatter, "config io error at {}: {error}", path.display())
            }
            ConfigError::Parse(error) => write!(formatter, "config parse error: {error}"),
            ConfigError::Encode(error) => write!(formatter, "config encode error: {error}"),
            ConfigError::Xml(message) => write!(formatter, "sizer xml error: {message}"),
            ConfigError::Invalid(problems) => {
                write!(formatter, "config has {} problem(s):", problems.len())?;
                for problem in problems {
                    write!(formatter, "\n  {}: {}", problem.field, problem.message)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        ConfigError::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        ConfigError::Parse(error)
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(error: toml::ser::Error) -> Self {
        ConfigError::Encode(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version = 1

[options]
area = "monitor"
basis = "frame"
tray = false

[[presets]]
label = "1080p"
size = "1920x1080"
anchor = "center"
hotkey = "Ctrl+Alt+1"

[[presets]]
label = "half"
size = "50%x50%"
monitor = "primary"
"#;

    #[test]
    fn parses_sample_and_round_trips() {
        let config = Config::from_toml(SAMPLE).expect("sample must parse");
        assert_eq!(config.options.area, AreaKind::Monitor);
        assert_eq!(config.options.basis, SizeBasis::Frame);
        assert!(!config.options.tray);
        assert_eq!(config.presets.len(), 2);
        assert_eq!(
            config.presets[0].size_spec().unwrap(),
            SizeSpec::Pixels(1920, 1080)
        );
        assert_eq!(
            config.presets[1].size_spec().unwrap(),
            SizeSpec::Percent(50.0, 50.0)
        );
        assert_eq!(
            config.presets[1].monitor_selector().unwrap(),
            MonitorSelector::Primary
        );

        let encoded = config.to_toml().expect("encode");
        let decoded = Config::from_toml(&encoded).expect("reparse");
        assert_eq!(config, decoded);
        assert!(config.validate().is_empty());
    }

    #[test]
    fn defaults_are_valid() {
        let config = Config::default();
        assert!(config.validate().is_empty());
        assert_eq!(config.options.area, AreaKind::WorkArea);
        assert_eq!(config.options.basis, SizeBasis::Client);
    }

    #[test]
    fn builtin_presets_are_valid_and_use_the_menu_hotkey() {
        use crate::menu::{MenuNode, menu};

        let config = Config::builtin();
        assert!(config.validate().is_empty());
        assert_eq!(config.options.hotkey.as_deref(), Some(BUILTIN_HOTKEY));
        assert_eq!(config.options.basis, SizeBasis::Client);
        assert_eq!(config.presets.len(), 14);
        // The first row only centres the window, so it keeps the current size.
        assert!(config.presets[0].is_move_only());
        assert_eq!(
            config.presets[1].size_spec().unwrap(),
            SizeSpec::Percent(50.0, 50.0)
        );
        // Sizes are sectioned by aspect ratio.
        let groups: Vec<String> = menu(&config.presets)
            .into_iter()
            .filter_map(|node| match node {
                MenuNode::Group { label, .. } => Some(label),
                _ => None,
            })
            .collect();
        assert_eq!(groups, ["4:3", "16:9", "16:10"]);
        assert!(
            config
                .presets
                .iter()
                .all(|preset| preset.anchor == Some(Anchor::Center))
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let error = Config::from_toml("version = 1\nsurprise = true\n").unwrap_err();
        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn absolute_position_places_from_the_area_origin() {
        let config = Config::from_toml(
            r#"
version = 1
[options]
area = "monitor"
basis = "frame"

[[presets]]
label = "corner"
size = "500x400"
x = 120
y = -40
monitor = "primary"
"#,
        )
        .unwrap();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
        let preset = &config.presets[0];
        assert_eq!(preset.position(), Some((120, -40)));
        assert_eq!(
            preset.place(Rect::new(1600, 0, 1920, 1080), (500, 400)),
            Some(Rect::new(1720, -40, 500, 400))
        );

        let encoded = config.to_toml().expect("encode");
        let decoded = Config::from_toml(&encoded).expect("reparse");
        assert_eq!(decoded.presets[0].position(), Some((120, -40)));
        assert_eq!(
            decoded.presets[0].monitor_selector().unwrap(),
            MonitorSelector::Primary
        );
    }

    #[test]
    fn an_anchor_without_a_size_keeps_the_current_size() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = "center"
size = ""
anchor = "center"
"#,
        )
        .unwrap();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
        let preset = &config.presets[0];
        assert!(preset.is_move_only());
        assert_eq!(preset.position(), None);
        assert_eq!(
            preset.place(Rect::new(0, 0, 1920, 1080), (800, 600)),
            Some(Rect::new(560, 240, 800, 600))
        );
    }

    #[test]
    fn an_empty_size_without_a_position_or_anchor_is_rejected() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = "nowhere"
size = ""
"#,
        )
        .unwrap();
        let problems = config.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].field, "presets[0].size");
        assert!(problems[0].message.contains("anchor"), "{problems:?}");
    }

    #[test]
    fn half_a_position_or_an_anchor_beside_it_is_rejected() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = "only-x"
size = "500x400"
x = 10
[[presets]]
label = "both"
size = "500x400"
anchor = "center"
x = 10
y = 20
"#,
        )
        .unwrap();
        let problems = config.validate();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.field == "presets[0].x" && p.message.contains("together")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.field == "presets[1].anchor" && p.message.contains("mutually exclusive")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_separator_cannot_carry_a_position() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = ""
size = ""
kind = "separator"
x = 10
y = 20
"#,
        )
        .unwrap();
        let problems = config.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].message.contains("position"));
    }

    #[test]
    fn duplicate_labels_and_bad_sizes_are_reported() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = "same"
size = "800x600"
[[presets]]
label = "Same"
size = "800 by 600"
"#,
        )
        .unwrap();
        let problems = config.validate();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.field.ends_with(".label")));
        assert!(problems.iter().any(|p| p.field.ends_with(".size")));
    }

    #[test]
    fn version_mismatch_is_reported() {
        let config = Config::from_toml("version = 99\n").unwrap();
        assert_eq!(config.validate()[0].field, "version");
    }

    #[test]
    fn empty_hotkey_is_reported() {
        let config = Config::from_toml(
            "version = 1\n[[presets]]\nlabel = \"x\"\nsize = \"10x10\"\nhotkey = \"  \"\n",
        )
        .unwrap();
        assert!(
            config
                .validate()
                .iter()
                .any(|p| p.field.ends_with(".hotkey"))
        );
    }

    #[test]
    fn config_path_prefers_xdg() {
        assert_eq!(
            config_path(Some("/xdg".into()), Some("/home/user".into())),
            Some(PathBuf::from("/xdg/sizer/config.toml"))
        );
        assert_eq!(
            config_path(None, Some("/home/user".into())),
            Some(PathBuf::from("/home/user/.config/sizer/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn save_then_load() {
        let directory = std::env::temp_dir().join(format!("sizer-config-{}", std::process::id()));
        let path = directory.join("nested").join("config.toml");
        let config = Config::from_toml(SAMPLE).unwrap();
        config.save(&path).expect("save");
        assert_eq!(Config::load(&path).unwrap(), config);
        // The atomic write must not leave its temporary file behind.
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn io_errors_name_the_file() {
        let missing = std::env::temp_dir()
            .join(format!("sizer-missing-{}", std::process::id()))
            .join("config.toml");
        let message = Config::load(&missing).unwrap_err().to_string();
        assert!(
            message.contains(&missing.display().to_string()),
            "{message}"
        );
    }
}
