//! Keeps the GNOME shell extension in step with the configuration file.
//!
//! The extension owns the menu, the hotkey and the geometry write; this process
//! is the only piece that reads TOML and pushes the result over D-Bus
//! (`SetPresets`). It is a standalone foreground `sizer daemon` rather than a
//! mode of `hotkeys` or `tray`: those two are X11-bound and cannot run on GNOME
//! Wayland, which is the platform where the extension is the primary menu.
//!
//! Loop, once every [`TICK`]:
//!
//! - the file stamp changed: reload; a missing file means the built-in presets;
//!   a broken file keeps the last good presets and reports the error;
//! - the shell reports a different preset list than ours (extension restart,
//!   another client calling `SetPresets`): push again;
//! - a push failed: retry next tick. Until a push succeeds the extension keeps
//!   its own built-in presets, so the menu is never empty.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use sizer_config::Config;
use zbus::blocking::{Connection, Proxy};

use crate::{CliResult, config_file_path, load_valid_config};

const BUS_NAME: &str = "org.sizerlinux.Shell";
const OBJECT_PATH: &str = "/org/sizerlinux/Shell";
const INTERFACE: &str = "org.sizerlinux.Shell";
const TICK: Duration = Duration::from_millis(500);

/// `(mtime seconds, mtime nanoseconds, size, inode)`; enough to notice a rewrite,
/// including an atomic temp+rename that keeps the same second and size.
type Stamp = (u64, u32, u64, u64);

#[derive(Debug, Deserialize)]
struct MenuState {
    #[serde(default)]
    presets: Vec<String>,
}

/// `SetPresets` reply. The extension validates each entry and reports what it
/// kept plus what it skipped, so the daemon can report the truth instead of
/// assuming every configured preset reached the menu.
#[derive(Debug, Deserialize)]
struct PushReply {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    presets: usize,
    #[serde(default)]
    skipped: Vec<Value>,
    #[serde(default)]
    error: Option<String>,
}

pub fn run(explicit_config: Option<&Path>) -> CliResult<()> {
    let connection = Connection::session()
        .map_err(|error| format!("cannot connect to the session bus: {error}"))?;
    let shell = Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)?;
    let path = config_file_path(explicit_config)?;
    let mut config = Config::builtin();
    let mut source = Source::Builtin;
    let mut file_stamp = stamp(&path);
    let mut dirty = true;
    let mut reported_down = false;
    let mut reported_error: Option<String> = None;
    // What the menu actually holds, as last reported by the extension. A config
    // whose entries the extension skips must not look permanently out of sync,
    // otherwise every tick would push again.
    let mut expected: Vec<String> = Vec::new();

    match file_stamp {
        None => println!(
            "sizer: no configuration file at {}; using built-in presets",
            path.display()
        ),
        Some(_) => match load_valid_config(&path) {
            Ok(loaded) => {
                config = loaded;
                source = Source::File;
                warn_about_hotkey(&config);
                println!("sizer: watching {}", path.display());
            }
            Err(error) => eprintln!("sizer: {}: {error}; using built-in presets", path.display()),
        },
    }

    loop {
        let current_stamp = stamp(&path);
        if current_stamp != file_stamp {
            file_stamp = current_stamp;
            match current_stamp {
                None => {
                    config = Config::builtin();
                    source = Source::Builtin;
                    dirty = true;
                    println!(
                        "sizer: {} disappeared; using built-in presets",
                        path.display()
                    );
                }
                Some(_) => match load_valid_config(&path) {
                    Ok(loaded) => {
                        config = loaded;
                        source = Source::File;
                        dirty = true;
                        reported_error = None;
                        warn_about_hotkey(&config);
                        println!("sizer: configuration changed; pushing new presets");
                    }
                    Err(error) => {
                        // Keep the last good presets: a half-written file must
                        // not wipe the menu.
                        let message = format!("sizer: {}: {error}", path.display());
                        if reported_error.as_deref() != Some(message.as_str()) {
                            eprintln!("{message}; keeping the last good presets");
                            reported_error = Some(message);
                        }
                    }
                },
            }
        }

        match shell.call::<_, _, String>("MenuState", &()) {
            Ok(reply) => {
                if reported_down {
                    println!("sizer: shell extension is back");
                    reported_down = false;
                    dirty = true;
                }
                if state_labels(&reply) != expected {
                    // The extension restarted or someone replaced its presets.
                    dirty = true;
                }
                if dirty {
                    match shell.call::<_, _, String>("SetPresets", &(payload(&config),)) {
                        Ok(reply) => match serde_json::from_str::<PushReply>(&reply) {
                            Ok(parsed) if parsed.ok => {
                                println!(
                                    "sizer: pushed {} of {} presets from {}",
                                    parsed.presets,
                                    config.presets.len(),
                                    source.describe(&path)
                                );
                                report_skipped(&parsed.skipped);
                                expected = current_labels(&shell);
                                dirty = false;
                            }
                            Ok(parsed) => {
                                eprintln!(
                                    "sizer: the shell extension rejected the preset list ({}); \
                                     its menu keeps the previous presets",
                                    parsed.error.as_deref().unwrap_or("unknown error")
                                );
                                report_skipped(&parsed.skipped);
                                expected = current_labels(&shell);
                                dirty = false;
                            }
                            Err(error) => {
                                // An unreadable reply must not cause a retry storm
                                // twice a second; report it once per change instead.
                                eprintln!(
                                    "sizer: cannot read the SetPresets reply ({error}): {reply}"
                                );
                                expected = current_labels(&shell);
                                dirty = false;
                            }
                        },
                        Err(error) => {
                            eprintln!("sizer: SetPresets failed: {error}");
                        }
                    }
                }
            }
            Err(error) => {
                if !reported_down {
                    eprintln!("sizer: waiting for the GNOME shell extension ({BUS_NAME}): {error}");
                    reported_down = true;
                }
            }
        }
        std::thread::sleep(TICK);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Builtin,
    File,
}

impl Source {
    fn describe(self, path: &Path) -> String {
        match self {
            Source::Builtin => "built-in presets".to_string(),
            Source::File => path.display().to_string(),
        }
    }
}

fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    // The inode catches the atomic temp+rename save even when mtime
    // granularity or file size happen to be identical.
    Some((
        since.as_secs(),
        since.subsec_nanos(),
        metadata.len(),
        metadata.ino(),
    ))
}

/// `SetPresets` payload.
///
/// Version 2 is an object so the preset fields the menu needs, the per-preset
/// monitor and the global `[options]` reach the extension. An extension that
/// only understands the old array replies `expected_array` instead of silently
/// dropping the extra fields; the push reporter surfaces that.
fn payload(config: &Config) -> String {
    let presets: Vec<Value> = config
        .presets
        .iter()
        .map(|preset| {
            json!({
                "label": preset.label,
                "size": preset.size,
                "anchor": preset.anchor,
                "group": preset.group,
                "monitor": preset.monitor,
                "x": preset.x,
                "y": preset.y,
                "allow_outside": preset.allow_outside_bounds,
                "kind": if preset.is_separator() { "separator" } else { "resize" },
            })
        })
        .collect();
    let options = json!({
        "area": area_label(config.options.area),
        "basis": basis_label(config.options.basis),
        "hotkey": extension_hotkey(config),
    });
    serde_json::to_string(&json!({"version": 2, "presets": presets, "options": options}))
        .unwrap_or_else(|_| "{\"presets\":[]}".to_string())
}

fn area_label(area: sizer_core::AreaKind) -> &'static str {
    match area {
        sizer_core::AreaKind::WorkArea => "workarea",
        sizer_core::AreaKind::Monitor => "monitor",
    }
}

fn basis_label(basis: sizer_core::SizeBasis) -> &'static str {
    match basis {
        sizer_core::SizeBasis::Client => "client",
        sizer_core::SizeBasis::Frame => "frame",
    }
}

/// The menu hotkey, but only when it is a valid accelerator: handing a broken
/// one to the shell would leave the menu unreachable with no visible cause.
fn extension_hotkey(config: &Config) -> Option<&str> {
    config
        .options
        .hotkey
        .as_deref()
        .filter(|hotkey| sizer_backend_x11::validate_accelerator(hotkey).is_ok())
}

/// Report a hotkey the extension cannot bind, once per configuration change.
fn warn_about_hotkey(config: &Config) {
    if let Some(hotkey) = config.options.hotkey.as_deref()
        && let Err(message) = sizer_backend_x11::validate_accelerator(hotkey)
    {
        eprintln!(
            "sizer: options.hotkey: {message}; the GNOME extension keeps its current keybinding"
        );
    }
}

fn state_labels(reply: &str) -> Vec<String> {
    serde_json::from_str::<MenuState>(reply)
        .map(|state| state.presets)
        .unwrap_or_default()
}

/// Labels the extension reports right now, used as the sync baseline.
fn current_labels(shell: &Proxy<'_>) -> Vec<String> {
    shell
        .call::<_, _, String>("MenuState", &())
        .map(|reply| state_labels(&reply))
        .unwrap_or_default()
}

/// Report each entry the extension refused, with its label and reason.
///
/// The extension says `{label, reason}`; older builds sent a bare label or
/// null, which is still reported instead of being dropped.
fn report_skipped(skipped: &[Value]) {
    for entry in skipped {
        let label = entry
            .get("label")
            .and_then(Value::as_str)
            .or_else(|| entry.as_str())
            .unwrap_or("<unnamed>");
        let reason = entry
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("invalid preset");
        eprintln!("sizer: the shell extension skipped {label:?}: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sizer_config::Preset;
    use sizer_config::PresetKind;
    use sizer_config::menu::{menu, preset_labels};
    use sizer_core::Anchor;

    fn config() -> Config {
        Config {
            presets: vec![
                Preset {
                    label: "1080p".to_string(),
                    size: "1920x1080".to_string(),
                    group: Some("16:9".to_string()),
                    anchor: Some(Anchor::Center),
                    hotkey: None,
                    monitor: None,
                    x: None,
                    y: None,
                    allow_outside_bounds: None,
                    kind: PresetKind::Resize,
                },
                Preset {
                    label: "half".to_string(),
                    size: "50%x50%".to_string(),
                    group: None,
                    anchor: None,
                    hotkey: None,
                    monitor: None,
                    x: None,
                    y: None,
                    allow_outside_bounds: None,
                    kind: PresetKind::Resize,
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn payload_carries_every_field_the_menu_needs() {
        let payload = payload(&config());
        let parsed: Value = serde_json::from_str(&payload).expect("json");
        assert_eq!(
            parsed,
            json!({
                "version": 2,
                "presets": [
                    {
                        "label": "1080p",
                        "size": "1920x1080",
                        "anchor": "center",
                        "group": "16:9",
                        "monitor": null,
                        "x": null,
                        "y": null,
                        "allow_outside": null,
                        "kind": "resize",
                    },
                    {
                        "label": "half",
                        "size": "50%x50%",
                        "anchor": null,
                        "group": null,
                        "monitor": null,
                        "x": null,
                        "y": null,
                        "allow_outside": null,
                        "kind": "resize",
                    },
                ],
                "options": {"area": "workarea", "basis": "client", "hotkey": null},
            })
        );
    }

    #[test]
    fn payload_pushes_monitor_basis_area_and_hotkey() {
        let mut config = config();
        config.options.area = sizer_core::AreaKind::Monitor;
        config.options.basis = sizer_core::SizeBasis::Frame;
        config.options.hotkey = Some("<Super><Alt>z".to_string());
        config.presets[1].monitor = Some("primary".to_string());
        let parsed: Value = serde_json::from_str(&payload(&config)).expect("json");
        assert_eq!(
            parsed["options"],
            json!({"area": "monitor", "basis": "frame", "hotkey": "<Super><Alt>z"})
        );
        assert_eq!(parsed["presets"][1]["monitor"], json!("primary"));
    }

    #[test]
    fn payload_carries_an_absolute_position() {
        let mut config = config();
        config.presets[0].anchor = None;
        config.presets[0].x = Some(-120);
        config.presets[0].y = Some(64);
        let parsed: Value = serde_json::from_str(&payload(&config)).expect("json");
        assert_eq!(parsed["presets"][0]["x"], json!(-120));
        assert_eq!(parsed["presets"][0]["y"], json!(64));
    }

    #[test]
    fn payload_withholds_a_hotkey_the_shell_cannot_bind() {
        let mut config = config();
        config.options.hotkey = Some("<Hyper>q".to_string());
        let parsed: Value = serde_json::from_str(&payload(&config)).expect("json");
        assert_eq!(parsed["options"]["hotkey"], Value::Null);
    }

    #[test]
    fn push_reply_reports_what_the_extension_kept() {
        let reply: PushReply = serde_json::from_str(
            r#"{"ok":true,"presets":1,"groups":0,
                "skipped":[{"label":"bad","reason":"unknown anchor \"nope\""}]}"#,
        )
        .expect("json");
        assert!(reply.ok);
        assert_eq!(reply.presets, 1);
        assert_eq!(reply.skipped.len(), 1);
        assert_eq!(reply.skipped[0]["reason"], "unknown anchor \"nope\"");
    }

    #[test]
    fn a_rejected_push_reply_is_readable() {
        let reply: PushReply =
            serde_json::from_str(r#"{"ok":false,"error":"expected_presets_array"}"#).expect("json");
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("expected_presets_array"));
        assert!(reply.skipped.is_empty());
    }

    #[test]
    fn skipped_entries_of_older_extensions_still_parse() {
        // Older replies carried bare labels or null.
        let reply: PushReply =
            serde_json::from_str(r#"{"ok":true,"presets":1,"skipped":["half",null]}"#)
                .expect("json");
        assert_eq!(reply.skipped.len(), 2);
    }

    #[test]
    fn expected_labels_follow_the_shared_menu_order() {
        let labels = preset_labels(&menu(&config().presets));
        assert_eq!(labels, vec!["1080p".to_string(), "half".to_string()]);
    }

    #[test]
    fn menu_state_labels_are_read_leniently() {
        assert_eq!(
            state_labels(r#"{"open":true,"presets":["a","b"]}"#),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(state_labels("not json").is_empty());
        assert!(state_labels("{}").is_empty());
    }

    #[test]
    fn stamp_is_none_for_a_missing_file() {
        assert_eq!(stamp(Path::new("/nonexistent/sizer/config.toml")), None);
    }
}
