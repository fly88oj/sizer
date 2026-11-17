//! Import and export of Sizer's `sizer.xml`.
//!
//! Only data that has a Linux meaning is carried over. Sizer stores Win32
//! virtual key codes, DWM frame compensation and nested menu groups; the first
//! two are translated or dropped, groups are flattened into ` / ` separated
//! preset groups, and elements without a Linux equivalent are reported instead
//! of silently disappearing.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::{Writer, XmlVersion};
use serde::Serialize;

use crate::{Config, ConfigError, Options, Preset};

/// Sizer modifier bits used by `hotKeyModifiers`.
const SIZER_MOD_ALT: u32 = 1;
const SIZER_MOD_CONTROL: u32 = 2;
const SIZER_MOD_SHIFT: u32 = 4;
const SIZER_MOD_WIN: u32 = 8;

/// An element that could not be represented as a preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedItem {
    pub element: String,
    pub label: String,
    pub reason: String,
}

/// Result of reading a Sizer file: what became a preset and what did not.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportReport {
    pub config: Config,
    pub skipped: Vec<SkippedItem>,
}

/// Read a Sizer `sizer.xml` document.
pub fn import(xml: &str) -> Result<ImportReport, ConfigError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut report = ImportReport {
        config: Config::default(),
        skipped: Vec::new(),
    };
    let mut groups: Vec<String> = Vec::new();
    let mut skip_depth = 0usize;
    let mut root = None;
    let mut buffer = Vec::new();

    loop {
        let event = reader.read_event_into(&mut buffer).map_err(|error| {
            ConfigError::Xml(format!("at byte {}: {error}", reader.buffer_position()))
        })?;
        let eof = matches!(event, Event::Eof);
        match event {
            Event::Start(element) => {
                if root.is_none() {
                    root = Some(element.local_name().as_ref().to_vec());
                }
                if skip_depth > 0 {
                    skip_depth += 1;
                } else {
                    let group = groups.last().cloned();
                    skip_depth += handle(&element, group.as_deref(), &mut report, &mut groups);
                }
            }
            Event::Empty(element) => {
                if root.is_none() {
                    root = Some(element.local_name().as_ref().to_vec());
                }
                if skip_depth == 0 {
                    let group = groups.last().cloned();
                    handle(&element, group.as_deref(), &mut report, &mut groups);
                }
            }
            Event::End(element) => {
                if skip_depth > 0 {
                    skip_depth -= 1;
                } else if element.local_name().as_ref() == b"group" {
                    groups.pop();
                }
            }
            _ => {}
        }
        buffer.clear();
        if eof {
            break;
        }
    }

    match root.as_deref() {
        Some(b"sizer") => Ok(report),
        Some(other) => Err(ConfigError::Xml(format!(
            "root element is <{}>, expected <sizer>",
            String::from_utf8_lossy(other)
        ))),
        None => Err(ConfigError::Xml("document has no elements".to_string())),
    }
}

/// Process one element, returning the subtree depth that has to be skipped.
fn handle(
    element: &BytesStart<'_>,
    group: Option<&str>,
    report: &mut ImportReport,
    groups: &mut Vec<String>,
) -> usize {
    match element.local_name().as_ref() {
        b"group" => {
            let name = attribute(element, b"description")
                .map(|text| normalize_label(&text))
                .unwrap_or_default();
            let path = match groups.last() {
                Some(parent) if !name.is_empty() => format!("{parent} / {name}"),
                _ => name,
            };
            groups.push(path);
            0
        }
        b"options" => {
            apply_options(element, &mut report.config.options);
            0
        }
        b"resize" => {
            let label = attribute(element, b"description")
                .map(|text| normalize_label(&text))
                .unwrap_or_default();
            match resize_preset(element, group) {
                Ok(preset) => report.config.presets.push(preset),
                Err(reason) => report.skipped.push(SkippedItem {
                    element: "resize".to_string(),
                    label,
                    reason,
                }),
            }
            0
        }
        b"separator" => {
            report.config.presets.push(Preset::separator(
                group.filter(|name| !name.is_empty()).map(str::to_string),
            ));
            0
        }
        b"template" | b"configDialog" => {
            report.skipped.push(SkippedItem {
                element: String::from_utf8_lossy(element.local_name().as_ref()).into_owned(),
                label: attribute(element, b"description")
                    .map(|text| normalize_label(&text))
                    .unwrap_or_default(),
                reason: "a Windows-only menu entry (launches a program or opens Sizer's own \
                         configuration dialog) and has no Linux equivalent; not imported"
                    .to_string(),
            });
            1
        }
        b"relocate" => {
            let label = attribute(element, b"description")
                .map(|text| normalize_label(&text))
                .unwrap_or_default();
            match relocate_preset(element, group) {
                Ok(preset) => report.config.presets.push(preset),
                Err(reason) => report.skipped.push(SkippedItem {
                    element: "relocate".to_string(),
                    label,
                    reason,
                }),
            }
            0
        }
        b"macro" => {
            report.skipped.push(SkippedItem {
                element: "macro".to_string(),
                label: attribute(element, b"description")
                    .map(|text| normalize_label(&text))
                    .unwrap_or_default(),
                reason: "has no Linux equivalent and was not imported".to_string(),
            });
            1
        }
        _ => 0,
    }
}

fn resize_preset(element: &BytesStart<'_>, group: Option<&str>) -> Result<Preset, String> {
    let width = attribute(element, b"width")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| "width attribute is missing or not a positive integer".to_string())?;
    let height = attribute(element, b"height")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| "height attribute is missing or not a positive integer".to_string())?;
    let label = attribute(element, b"description")
        .map(|text| normalize_label(&text))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("{width}x{height}"));
    Ok(Preset {
        label,
        size: format!("{width}x{height}"),
        group: group.filter(|name| !name.is_empty()).map(str::to_string),
        anchor: None,
        hotkey: None,
        monitor: None,
        // Sizer's <resize> names a size; an absolute position is <relocate>.
        x: None,
        y: None,
        allow_outside_bounds: None,
        kind: crate::PresetKind::Resize,
    })
}

/// Sizer's `<relocate>`: absolute `left`/`top` inside the area, optionally with
/// `allowOutsideBounds`. Without `width`/`height` it only moves the window.
fn relocate_preset(element: &BytesStart<'_>, group: Option<&str>) -> Result<Preset, String> {
    let x = attribute(element, b"left")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .ok_or_else(|| "left attribute is missing or not an integer".to_string())?;
    let y = attribute(element, b"top")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .ok_or_else(|| "top attribute is missing or not an integer".to_string())?;
    let label = attribute(element, b"description")
        .map(|text| normalize_label(&text))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("{x},{y}"));
    let width = attribute(element, b"width")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| *value > 0);
    let height = attribute(element, b"height")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| *value > 0);
    let size = match (width, height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        (None, None) => String::new(),
        _ => return Err("width and height must be set together".to_string()),
    };
    Ok(Preset {
        label,
        size,
        group: group.filter(|name| !name.is_empty()).map(str::to_string),
        anchor: None,
        hotkey: None,
        monitor: attribute(element, b"monitor").filter(|text| !text.trim().is_empty()),
        x: Some(x),
        y: Some(y),
        allow_outside_bounds: attribute(element, b"allowOutsideBounds").map(|value| {
            let value = value.trim();
            !(value == "0" || value.eq_ignore_ascii_case("false"))
        }),
        kind: crate::PresetKind::Resize,
    })
}

fn apply_options(element: &BytesStart<'_>, options: &mut Options) {
    if let Some(value) = attribute(element, b"showIconInTray") {
        options.tray = value.trim() != "0";
    }
    let key = attribute(element, b"hotKey").and_then(|value| value.trim().parse::<u32>().ok());
    let modifiers = attribute(element, b"hotKeyModifiers")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if let Some(accelerator) = key.and_then(|key| accelerator_from_vk(key, modifiers)) {
        options.hotkey = Some(accelerator);
    }
}

/// Sizer stores menu text with embedded newlines so it renders on two lines.
fn normalize_label(text: &str) -> String {
    let lines: Vec<&str> = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    match lines.as_slice() {
        [] => String::new(),
        [single] => (*single).to_string(),
        [width, height]
            if width.chars().all(|ch| ch.is_ascii_digit())
                && height.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            format!("{width}x{height}")
        }
        many => many.join(" "),
    }
}

fn attribute(element: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .with_checks(false)
        .flatten()
        .find(|attribute| attribute.key.as_ref() == name)
        .and_then(|attribute| attribute.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Win32 virtual key code plus Sizer modifiers to an accelerator.
fn accelerator_from_vk(key: u32, modifiers: u32) -> Option<String> {
    let name = match key {
        0x30..=0x39 => char::from(b'0' + (key - 0x30) as u8).to_string(),
        0x41..=0x5a => char::from(b'a' + (key - 0x41) as u8).to_string(),
        0x70..=0x87 => format!("F{}", key - 0x70 + 1),
        _ => return None,
    };
    let mut text = String::new();
    for (bit, token) in [
        (SIZER_MOD_SHIFT, "Shift"),
        (SIZER_MOD_CONTROL, "Ctrl"),
        (SIZER_MOD_ALT, "Alt"),
        (SIZER_MOD_WIN, "Super"),
    ] {
        if modifiers & bit != 0 {
            text.push('<');
            text.push_str(token);
            text.push('>');
        }
    }
    text.push_str(&name);
    Some(text)
}

/// Accelerator to Win32 virtual key code plus Sizer modifiers.
fn vk_from_accelerator(text: &str) -> Option<(u32, u32)> {
    let parts: Vec<String> = if text.contains('<') {
        text.replace('>', "<")
            .split('<')
            .map(|chunk| chunk.trim().to_string())
            .filter(|chunk| !chunk.is_empty())
            .collect()
    } else {
        text.split('+')
            .map(|chunk| chunk.trim().to_string())
            .collect()
    };
    let mut modifiers = 0;
    let mut key = None;
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "shift" => modifiers |= SIZER_MOD_SHIFT,
            "ctrl" | "control" | "primary" => modifiers |= SIZER_MOD_CONTROL,
            "alt" | "mod1" => modifiers |= SIZER_MOD_ALT,
            "super" | "win" | "meta" | "mod4" => modifiers |= SIZER_MOD_WIN,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(vk_for_key(other)?);
            }
        }
    }
    key.map(|key| (key, modifiers))
}

fn vk_for_key(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix('F')
        && let Ok(number) = rest.parse::<u32>()
        && (1..=24).contains(&number)
    {
        return Some(0x70 + number - 1);
    }
    let mut chars = name.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii_alphanumeric() {
        return None;
    }
    Some(ch.to_ascii_uppercase() as u32)
}

/// Write a configuration as a Sizer `sizer.xml` document.
///
/// Percentage sizes cannot be expressed because Sizer only stores pixels.
pub fn export(config: &Config) -> Result<String, ConfigError> {
    let mut document = SizerDocument::new();
    document.open_root()?;
    document.write_options(&config.options)?;
    document.open_menu()?;

    let mut open_group: Option<&str> = None;
    for preset in &config.presets {
        let group = preset.group.as_deref().filter(|name| !name.is_empty());
        if open_group != group {
            if open_group.is_some() {
                document.close_group()?;
            }
            if let Some(name) = group {
                document.open_group(name)?;
            }
            open_group = group;
        }
        if preset.is_separator() {
            document.write_separator()?;
        } else if preset.is_move_only() {
            if preset.position().is_none() {
                return Err(ConfigError::Xml(format!(
                    "preset {:?} keeps the current size and only names an anchor, which Sizer \
                     XML cannot express; keep it in TOML",
                    preset.label
                )));
            }
            document.write_relocate(preset, config.options.allow_outside_bounds)?;
        } else if preset.position().is_some() {
            return Err(ConfigError::Xml(format!(
                "preset {:?} both resizes and names a position, which Sizer XML cannot express; \
                 keep it in TOML",
                preset.label
            )));
        } else {
            document.write_resize(preset)?;
        }
    }
    if open_group.is_some() {
        document.close_group()?;
    }

    document.close_menu()?;
    Ok(document.finish())
}

/// Tiny wrapper that keeps the writer calls readable.
struct SizerDocument {
    writer: Writer<Vec<u8>>,
}

impl SizerDocument {
    fn new() -> Self {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Decl(BytesDecl::new(
                "1.0",
                Some("UTF-8"),
                Some("no"),
            )))
            .expect("writing to memory cannot fail");
        Self { writer }
    }

    fn open_root(&mut self) -> Result<(), ConfigError> {
        self.writer
            .write_event(Event::Start(BytesStart::new("sizer")))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn write_options(&mut self, options: &Options) -> Result<(), ConfigError> {
        let mut element = BytesStart::new("options");
        element.push_attribute(("showIconInTray", if options.tray { "1" } else { "0" }));
        if let Some(hotkey) = options.hotkey.as_deref() {
            let (key, modifiers) = vk_from_accelerator(hotkey).ok_or_else(|| {
                ConfigError::Xml(format!(
                    "hotkey {hotkey:?} cannot be expressed as a Win32 virtual key code"
                ))
            })?;
            element.push_attribute(("hotKey", key.to_string().as_str()));
            element.push_attribute(("hotKeyModifiers", modifiers.to_string().as_str()));
        }
        self.writer
            .write_event(Event::Empty(element))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn open_menu(&mut self) -> Result<(), ConfigError> {
        self.writer
            .write_event(Event::Start(BytesStart::new("menu")))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn close_menu(&mut self) -> Result<(), ConfigError> {
        self.writer
            .write_event(Event::End(BytesEnd::new("menu")))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn open_group(&mut self, name: &str) -> Result<(), ConfigError> {
        let mut element = BytesStart::new("group");
        element.push_attribute(("description", name));
        self.writer
            .write_event(Event::Start(element))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn close_group(&mut self) -> Result<(), ConfigError> {
        self.writer
            .write_event(Event::End(BytesEnd::new("group")))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn write_resize(&mut self, preset: &Preset) -> Result<(), ConfigError> {
        let (width, height) = match preset.size_spec().map_err(ConfigError::Xml)? {
            sizer_core::SizeSpec::Pixels(width, height) => (width, height),
            sizer_core::SizeSpec::Percent(_, _) => {
                return Err(ConfigError::Xml(format!(
                    "preset {:?} uses a percentage size, which Sizer XML cannot express",
                    preset.label
                )));
            }
        };
        let mut element = BytesStart::new("resize");
        element.push_attribute(("description", preset.label.as_str()));
        element.push_attribute(("width", width.to_string().as_str()));
        element.push_attribute(("height", height.to_string().as_str()));
        self.writer
            .write_event(Event::Empty(element))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    /// Sizer's `<relocate>`; `allowOutsideBounds` is only written when it is on,
    /// so an export does not add noise to every entry.
    fn write_relocate(
        &mut self,
        preset: &Preset,
        default_outside: bool,
    ) -> Result<(), ConfigError> {
        let (x, y) = preset
            .position()
            .expect("move-only presets carry a position");
        let mut element = BytesStart::new("relocate");
        element.push_attribute(("description", preset.label.as_str()));
        element.push_attribute(("left", x.to_string().as_str()));
        element.push_attribute(("top", y.to_string().as_str()));
        if let Some(monitor) = preset.monitor.as_deref().filter(|name| !name.is_empty()) {
            element.push_attribute(("monitor", monitor));
        }
        // Written only when it differs from the global default in effect, so an
        // export does not repeat the same value on every relocation.
        if let Some(on) = preset
            .allow_outside_bounds
            .filter(|on| *on != default_outside)
        {
            element.push_attribute(("allowOutsideBounds", if on { "1" } else { "0" }));
        }
        self.writer
            .write_event(Event::Empty(element))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn write_separator(&mut self) -> Result<(), ConfigError> {
        self.writer
            .write_event(Event::Empty(BytesStart::new("separator")))
            .expect("writing to memory cannot fail");
        Ok(())
    }

    fn finish(mut self) -> String {
        self.writer
            .write_event(Event::End(BytesEnd::new("sizer")))
            .expect("writing to memory cannot fail");
        String::from_utf8(self.writer.into_inner()).expect("writer only emits UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<sizer>
  <options showIconInTray="1" adjustOppositeEdge="0" tooltipOption="1"
           hotKey="90" hotKeyModifiers="10" hotKeyExtended="0"/>
  <menu>
    <resize description="640&#10;480" width="640" height="480" />
    <separator />
    <group description="16:9">
      <resize description="1280&#10;720" width="1280" height="720" />
      <group description="YouTube">
        <resize description="1080p" width="1920" height="1080" />
      </group>
    </group>
    <relocate description="top-right" top="0" left="0" monitor="0" />
    <macro description="grid" macro="resize ..." />
  </menu>
</sizer>
"#;

    #[test]
    fn imports_resize_groups_and_separators() {
        let report = import(SAMPLE).expect("sample must import");
        let presets = &report.config.presets;
        assert_eq!(presets.len(), 5, "{presets:?}");
        assert_eq!(presets[0].label, "640x480");
        assert_eq!(presets[0].size, "640x480");
        assert_eq!(presets[0].group, None);
        assert!(presets[1].is_separator());
        assert_eq!(presets[1].group, None);
        assert_eq!(presets[2].label, "1280x720");
        assert_eq!(presets[2].group.as_deref(), Some("16:9"));
        assert_eq!(presets[3].label, "1080p");
        assert_eq!(presets[3].group.as_deref(), Some("16:9 / YouTube"));
        // <relocate> is a move-only preset: a position, no size.
        assert_eq!(presets[4].label, "top-right");
        assert_eq!(presets[4].position(), Some((0, 0)));
        assert_eq!(presets[4].monitor.as_deref(), Some("0"));
        assert!(presets[4].is_move_only());
        assert_eq!(presets[4].allow_outside_bounds, None);
        assert!(report.config.validate().is_empty());
    }

    #[test]
    fn relocate_maps_the_outside_switch_and_round_trips() {
        let report = import(
            r#"<sizer><menu>
                <relocate description="off-screen" left="-40" top="700"
                          monitor="1" allowOutsideBounds="1" />
                <relocate description="inside" left="10" top="20"
                          allowOutsideBounds="0" />
            </menu></sizer>"#,
        )
        .expect("document parses");
        let first = &report.config.presets[0];
        assert_eq!(first.position(), Some((-40, 700)));
        assert_eq!(first.allow_outside_bounds, Some(true));
        assert!(first.allows_outside(false));
        // An explicit false is kept as "not allowed", not as "follow the option".
        assert_eq!(report.config.presets[1].allow_outside_bounds, Some(false));

        // With the default off, only the opted-in entry carries the attribute,
        // and the explicit false collapses into "follow the option".
        let xml = export(&report.config).expect("export");
        assert!(
            xml.contains(r#"<relocate description="off-screen" left="-40" top="700""#),
            "{xml}"
        );
        assert!(xml.contains(r#"allowOutsideBounds="1""#), "{xml}");
        assert_eq!(xml.matches("allowOutsideBounds").count(), 1, "{xml}");
        let again = import(&xml).expect("reimport");
        assert_eq!(again.config.presets[0].allow_outside_bounds, Some(true));
        assert_eq!(again.config.presets[1].allow_outside_bounds, None);

        // With the global switch on, the explicit false is the meaningful value
        // and must be written, so the pair round-trips either way.
        let mut for_export = report.config.clone();
        for_export.options.allow_outside_bounds = true;
        let xml = export(&for_export).expect("export");
        assert!(xml.contains(r#"allowOutsideBounds="0""#), "{xml}");
        assert!(!xml.contains(r#"allowOutsideBounds="1""#), "{xml}");
        let again = import(&xml).expect("reimport");
        assert_eq!(again.config.presets[1].allow_outside_bounds, Some(false));
        assert_eq!(again.config.presets[0].position(), Some((-40, 700)));
    }

    #[test]
    fn a_preset_that_resizes_and_moves_cannot_be_exported() {
        let config = Config::from_toml(
            r#"
version = 1
[[presets]]
label = "both"
size = "500x400"
x = 10
y = 20
"#,
        )
        .unwrap();
        let error = export(&config).expect_err("XML has no element for that");
        assert!(
            error
                .to_string()
                .contains("both resizes and names a position")
        );
    }

    #[test]
    fn reports_windows_only_menu_entries() {
        let report = import(
            "<sizer><menu><template description=\"launch\" /><configDialog />\
             </menu></sizer>",
        )
        .expect("document parses");
        assert!(report.config.presets.is_empty());
        let elements: Vec<&str> = report
            .skipped
            .iter()
            .map(|item| item.element.as_str())
            .collect();
        assert_eq!(elements, ["template", "configDialog"]);
        assert!(report.skipped[0].reason.contains("Windows-only"));
    }

    #[test]
    fn imports_options_and_translates_hotkey() {
        let report = import(SAMPLE).expect("sample must import");
        assert!(report.config.options.tray);
        assert_eq!(
            report.config.options.hotkey.as_deref(),
            Some("<Ctrl><Super>z")
        );
    }

    #[test]
    fn reports_unsupported_elements() {
        let report = import(SAMPLE).expect("sample must import");
        let elements: Vec<&str> = report
            .skipped
            .iter()
            .map(|item| item.element.as_str())
            .collect();
        assert_eq!(elements, ["macro"]);
        assert_eq!(report.skipped[0].label, "grid");
    }

    #[test]
    fn rejects_foreign_document() {
        let error = import("<configuration><resize width=\"1\" height=\"1\"/></configuration>")
            .unwrap_err();
        assert!(matches!(error, ConfigError::Xml(_)), "{error}");
        assert!(import("").is_err());
    }

    #[test]
    fn reports_broken_resize() {
        let report = import(
            "<sizer><menu><resize description=\"bad\" width=\"0\" height=\"10\"/></menu></sizer>",
        )
        .expect("document parses");
        assert!(report.config.presets.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].element, "resize");
        assert_eq!(report.skipped[0].label, "bad");
    }

    #[test]
    fn exports_and_reimports() {
        let report = import(SAMPLE).expect("sample must import");
        let mut for_export = report.config.clone();
        // With the global switch on, an explicit `false` is the meaningful value
        // and must be written; with it off, the default is already off.
        for_export.options.allow_outside_bounds = true;
        let xml = export(&for_export).expect("export");
        let second = import(&xml).expect("reimport");
        assert_eq!(report.config, second.config);
        assert!(second.skipped.is_empty(), "{:?}", second.skipped);
    }

    #[test]
    fn refuses_percent_sizes() {
        let config = Config {
            version: crate::CURRENT_VERSION,
            options: Options::default(),
            presets: vec![Preset {
                label: "half".to_string(),
                size: "50%x50%".to_string(),
                group: None,
                anchor: None,
                hotkey: None,
                monitor: None,
                x: None,
                y: None,
                allow_outside_bounds: None,
                kind: crate::PresetKind::Resize,
            }],
        };
        let error = export(&config).unwrap_err();
        assert!(matches!(error, ConfigError::Xml(_)), "{error}");
    }

    #[test]
    fn accelerator_and_vk_round_trip() {
        for (text, key, modifiers, canonical) in [
            ("<Ctrl><Super>z", 90, 10, "<Ctrl><Super>z"),
            ("Ctrl+Alt+1", 0x31, 3, "<Ctrl><Alt>1"),
            ("<Shift><Alt>F9", 0x78, 5, "<Shift><Alt>F9"),
        ] {
            assert_eq!(vk_from_accelerator(text), Some((key, modifiers)), "{text}");
            assert_eq!(
                accelerator_from_vk(key, modifiers).as_deref(),
                Some(canonical),
                "{text}"
            );
        }
        assert_eq!(vk_from_accelerator("<Super>backslash"), None);
        assert_eq!(accelerator_from_vk(0x0d, 0), None);
    }
}
