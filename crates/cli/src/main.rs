//! `sizer` command line: the scriptable face of the core geometry model.
//!
//! Phase 1 ships the X11 backend only. Every command fails loudly when the
//! platform cannot do what was asked instead of silently doing nothing.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::json;

use sizer_backend_x11::{ProbePolicy, X11Backend};
use sizer_config::{Config, ConfigError, Preset, Problem, sizer_xml};
use sizer_core::{
    Anchor, AreaKind, Geometry, GeometryResult, MonitorSelector, Rect, SizeBasis, SizeSpec,
    WindowId,
};
use sizer_platform::{
    BackendError, HotkeyBackend, HotkeyOutcome, HotkeySpec, Monitor, WindowBackend, WindowSelector,
    resolve_selector,
};

mod configure;
mod daemon;
mod i18n;
mod macro_cmd;

type CliResult<T> = Result<T, Box<dyn Error>>;

// `--version` names the license and holder; `--help` stays free of copyright
// boilerplate, the same split the other command line tools on the desktop use.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " \u{2014} MIT License, Copyright (c) 2026 Jowency"
);

#[derive(Parser, Debug)]
#[command(
    name = "sizer",
    version = VERSION,
    about = "Precise window sizing for Linux (X11 windows; GNOME Wayland through the shell extension)"
)]
struct Cli {
    /// Machine readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Configuration file; defaults to the XDG location.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List controllable windows.
    List,
    /// Show frame and client geometry of one window.
    Geometry {
        /// active, <id>, 0x<id>, title:<text> or app:<class>
        #[arg(long, default_value = "active")]
        window: String,
        /// client or frame
        #[arg(long, default_value = "client")]
        basis: String,
    },
    /// Resize a window, optionally anchoring it inside the reference area.
    Resize {
        /// Pixel size, e.g. 1024x768.
        #[arg(long, value_name = "WxH", conflicts_with = "percent")]
        size: Option<String>,
        /// Percentage of the reference area, e.g. 50x50.
        #[arg(long, value_name = "WxH", conflicts_with = "size")]
        percent: Option<String>,
        /// Anchor inside the area; omitted keeps the current position.
        #[arg(long, default_value = "")]
        anchor: String,
        /// Absolute x inside the area; needs --y and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        x: Option<i32>,
        /// Absolute y inside the area; needs --x and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        y: Option<i32>,
        #[arg(long, default_value = "active")]
        window: String,
        /// active, primary or an index.
        #[arg(long, default_value = "active")]
        monitor: String,
        /// workarea or monitor
        #[arg(long, default_value = "workarea")]
        area: String,
        /// client or frame
        #[arg(long, default_value = "client")]
        basis: String,
        /// Let the window leave the area.
        #[arg(long, overrides_with = "no_allow_outside")]
        allow_outside: bool,
        /// Pull the window into the area.
        #[arg(long, overrides_with = "allow_outside")]
        no_allow_outside: bool,
    },
    /// Move a window without changing its size.
    Move {
        /// Anchor inside the area; needs --x/--y when omitted.
        #[arg(long, default_value = "")]
        anchor: String,
        /// Absolute x inside the area; needs --y and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        x: Option<i32>,
        /// Absolute y inside the area; needs --x and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        y: Option<i32>,
        #[arg(long, default_value = "active")]
        window: String,
        #[arg(long, default_value = "active")]
        monitor: String,
        #[arg(long, default_value = "workarea")]
        area: String,
        /// client or frame
        #[arg(long, default_value = "client")]
        basis: String,
        /// Let the window leave the area.
        #[arg(long, overrides_with = "no_allow_outside")]
        allow_outside: bool,
        /// Pull the window into the area.
        #[arg(long, overrides_with = "allow_outside")]
        no_allow_outside: bool,
    },
    /// Keep a window above others, or turn that off.
    Above {
        #[arg(long)]
        off: bool,
        #[arg(long, default_value = "active")]
        window: String,
    },
    /// Maximize a window, or restore it with --off.
    State {
        #[arg(long)]
        off: bool,
        #[arg(long, default_value = "active")]
        window: String,
    },
    /// Register a global hotkey and report whether delivery can be proven.
    Hotkey {
        /// Same default as the configured menu hotkey.
        #[arg(long, default_value = "<Primary><Super>z")]
        accelerator: String,
        /// Inject the combination once through XTEST to prove delivery.
        /// Only use on a display where typing is harmless.
        #[arg(long)]
        verify_delivery: bool,
        /// Keep the grab and report triggering for this many seconds.
        #[arg(long, default_value_t = 0)]
        hold: u64,
    },
    /// Diagnose session, backend capabilities and monitors.
    Doctor,
    /// Apply a configured preset to a window.
    Apply {
        /// Preset label, optionally prefixed with its group (`16:9 / 1280x720`).
        label: String,
        #[arg(long, default_value = "active")]
        window: String,
        /// Override the preset monitor: active, primary or an index.
        #[arg(long)]
        monitor: Option<String>,
        /// Override the configured area: workarea or monitor.
        #[arg(long)]
        area: Option<String>,
        /// Override the configured basis: client or frame.
        #[arg(long)]
        basis: Option<String>,
        /// Let the window leave the area.
        #[arg(long, overrides_with = "no_allow_outside")]
        allow_outside: bool,
        /// Pull the window into the area.
        #[arg(long, overrides_with = "allow_outside")]
        no_allow_outside: bool,
    },
    /// Register every configured hotkey.
    Hotkeys {
        /// Inject one probe keystroke per hotkey to prove delivery.
        #[arg(long)]
        verify_delivery: bool,
        /// Stay for this many seconds, applying the preset behind each hotkey.
        #[arg(long, default_value_t = 0)]
        hold: u64,
        #[arg(long, default_value = "active")]
        window: String,
    },
    /// Open the preset menu at the pointer: through the shell extension on
    /// GNOME, through the cursor menu on X11. This is what the desktop entry runs.
    Menu,
    /// Open the preset menu at the pointer and apply the chosen preset.
    /// Equivalent of Sizer's `-popup`; kept as the scriptable/debug entry point.
    Popup {
        /// Window to resize; resolved once a preset is chosen.
        #[arg(long, default_value = "active")]
        window: String,
    },
    /// Show a StatusNotifierItem tray with one entry per preset.
    Tray {
        /// Exit after this many seconds; 0 keeps running.
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Push presets to the GNOME shell extension and keep them in sync.
    ///
    /// Runs in the foreground: re-pushes when the configuration file changes,
    /// when the extension comes back, and when the shell lost the presets.
    Daemon,
    /// Inspect, validate and migrate the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Add, change, remove, reorder or capture presets.
    Preset {
        #[command(subcommand)]
        action: PresetAction,
    },
    /// Run a Sizer macro against matching windows.
    ///
    /// The macro text is the positional argument, or standard input when it is
    /// omitted. Every window is read back after the write, so the result reports
    /// achieved geometry instead of the request.
    Macro {
        /// Macro text; read from standard input when omitted.
        text: Option<String>,
        /// auto, x11 or gnome
        #[arg(long, default_value = "auto")]
        backend: String,
    },
}

#[derive(Subcommand, Debug)]
enum PresetAction {
    /// List presets in configuration order.
    List,
    /// Add a preset.
    Add {
        /// Preset name shown in the menu; Unicode is fine.
        #[arg(long)]
        label: String,
        /// 1024x768 or 50%x50%; omit it to keep the window's current size.
        #[arg(long, value_name = "WxH")]
        size: Option<String>,
        /// Menu group path, e.g. "16:9" or "16:9 / UHD".
        #[arg(long)]
        group: Option<String>,
        /// top_left, top, ..., bottom_right; also tl/t/tr/l/c/r/bl/b/br.
        #[arg(long)]
        anchor: Option<String>,
        /// Global hotkey for this preset, e.g. "<Super><Alt>F8".
        #[arg(long)]
        hotkey: Option<String>,
        /// active, primary or a monitor index.
        #[arg(long)]
        monitor: Option<String>,
        /// Absolute x inside the area (pixels); needs --y and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        x: Option<i32>,
        /// Absolute y inside the area (pixels); needs --x and excludes --anchor.
        #[arg(long, allow_hyphen_values = true)]
        y: Option<i32>,
        /// Insert at this 1-based position inside the preset's group.
        #[arg(long)]
        position: Option<usize>,
    },
    /// Change fields of an existing preset. An empty group, anchor, hotkey or
    /// monitor clears that field.
    Set {
        /// Preset label, or `group / label` when the label is ambiguous.
        selector: String,
        /// New preset name.
        #[arg(long)]
        label: Option<String>,
        /// New size: 1024x768 or 50%x50%.
        #[arg(long, value_name = "WxH")]
        size: Option<String>,
        /// New menu group path.
        #[arg(long)]
        group: Option<String>,
        /// New anchor, or an empty value to clear it.
        #[arg(long)]
        anchor: Option<String>,
        /// New hotkey, or an empty value to clear it.
        #[arg(long)]
        hotkey: Option<String>,
        /// New monitor (active/primary/index), or an empty value to clear it.
        #[arg(long)]
        monitor: Option<String>,
        /// Absolute x inside the area; needs --y and clears --anchor.
        #[arg(long, allow_hyphen_values = true)]
        x: Option<i32>,
        /// Absolute y inside the area; needs --x and clears --anchor.
        #[arg(long, allow_hyphen_values = true)]
        y: Option<i32>,
        /// Drop an absolute position and fall back to the anchor.
        #[arg(long)]
        no_position: bool,
    },
    /// Remove a preset.
    Remove {
        /// Preset label, or `group / label` when the label is ambiguous.
        selector: String,
    },
    /// Move a preset to another 1-based position inside its group.
    Move {
        /// Preset label, or `group / label` when the label is ambiguous.
        selector: String,
        #[arg(long)]
        position: usize,
    },
    /// Duplicate a preset. The copy starts without a hotkey, because two
    /// presets on one accelerator would be ambiguous.
    Copy {
        /// Preset label, or `group / label` when the label is ambiguous.
        selector: String,
        /// Name for the copy; defaults to "<label> copy".
        #[arg(long)]
        label: Option<String>,
        /// Menu group path for the copy; defaults to the source group.
        #[arg(long)]
        group: Option<String>,
        /// Absolute x for the copy; needs --y and replaces the source anchor.
        #[arg(long, allow_hyphen_values = true)]
        x: Option<i32>,
        /// Absolute y for the copy; needs --x and replaces the source anchor.
        #[arg(long, allow_hyphen_values = true)]
        y: Option<i32>,
        /// Insert at this 1-based position inside the target group.
        #[arg(long)]
        position: Option<usize>,
    },
    /// Insert a separator line into the menu.
    Separator {
        /// Menu group path the line belongs to.
        #[arg(long)]
        group: Option<String>,
        /// Insert at this 1-based position inside the group.
        #[arg(long)]
        position: Option<usize>,
    },
    /// Store the current size of a window as a new preset, in the configured
    /// basis so applying it again reproduces the same size.
    Save {
        /// Preset name shown in the menu; Unicode is fine.
        #[arg(long)]
        label: String,
        /// Menu group path, e.g. "16:9" or "16:9 / UHD".
        #[arg(long)]
        group: Option<String>,
        #[arg(long, default_value = "active")]
        window: String,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Print the configuration file path.
    Path,
    /// Validate the configuration file.
    Check,
    /// Print the configuration without changing it.
    Show,
    /// Import a Sizer sizer.xml file.
    Import {
        xml: PathBuf,
        /// Write the result to the configuration file instead of stdout.
        #[arg(long)]
        write: bool,
    },
    /// Export to Sizer sizer.xml.
    Export {
        /// Output file; stdout when omitted.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Edit presets in a configuration window.
    Edit,
}

fn main() {
    i18n::init();
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("sizer: {err}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> CliResult<()> {
    let json = cli.json;
    let config = cli.config;
    let config_path = config.as_deref();
    match cli.command {
        Command::List => cmd_list(json),
        Command::Geometry { window, basis } => cmd_geometry(json, &window, parse_basis(&basis)?),
        Command::Resize {
            size,
            percent,
            anchor,
            x,
            y,
            window,
            monitor,
            area,
            basis,
            allow_outside,
            no_allow_outside,
        } => cmd_resize(
            json,
            ResizeRequest {
                size,
                percent,
                anchor,
                x,
                y,
                window,
                monitor,
                area,
                basis: parse_basis(&basis)?,
                allow_outside: override_switch(allow_outside, no_allow_outside),
            },
            config_path,
        ),
        Command::Move {
            anchor,
            x,
            y,
            window,
            monitor,
            area,
            basis,
            allow_outside,
            no_allow_outside,
        } => cmd_move(
            json,
            MoveRequest {
                anchor,
                x,
                y,
                window,
                monitor,
                area,
                basis: parse_basis(&basis)?,
                allow_outside: override_switch(allow_outside, no_allow_outside),
            },
            config_path,
        ),
        Command::Above { off, window } => cmd_above(json, &window, !off),
        Command::State { off, window } => cmd_state(json, &window, !off),
        Command::Hotkey {
            accelerator,
            verify_delivery,
            hold,
        } => cmd_hotkey(json, &accelerator, verify_delivery, hold),
        Command::Doctor => cmd_doctor(json, config_path),
        Command::Apply {
            label,
            window,
            monitor,
            area,
            basis,
            allow_outside,
            no_allow_outside,
        } => cmd_apply(
            json,
            &label,
            &window,
            monitor.as_deref(),
            area.as_deref(),
            basis.as_deref(),
            override_switch(allow_outside, no_allow_outside),
            config_path,
        ),
        Command::Hotkeys {
            verify_delivery,
            hold,
            window,
        } => cmd_hotkeys(json, verify_delivery, hold, &window, config_path),
        Command::Popup { window } => cmd_popup(json, &window, config_path),
        Command::Menu => cmd_menu(json, config_path),
        Command::Tray { seconds } => cmd_tray(json, seconds, config_path),
        Command::Daemon => {
            // Structured output would be a lie: `daemon` is a long-running log
            // stream. Fail instead of accepting the flag and ignoring it.
            if json {
                return Err("daemon writes plain logs, not JSON; drop --json".into());
            }
            daemon::run(config_path)
        }
        Command::Config { action } => cmd_config(json, action, config_path),
        Command::Preset { action } => cmd_preset(json, action, config_path),
        Command::Macro { text, backend } => {
            let source = match text {
                Some(text) => text,
                None => read_macro_stdin()?,
            };
            macro_cmd::run(json, &source, macro_cmd::Backend::parse(&backend)?)
        }
    }
}

fn read_macro_stdin() -> CliResult<String> {
    use std::io::Read;
    let mut source = String::new();
    std::io::stdin().read_to_string(&mut source)?;
    if source.trim().is_empty() {
        return Err("no macro given and standard input is empty".into());
    }
    Ok(source)
}

struct ResizeRequest {
    size: Option<String>,
    percent: Option<String>,
    anchor: String,
    x: Option<i32>,
    y: Option<i32>,
    window: String,
    monitor: String,
    area: String,
    basis: SizeBasis,
    allow_outside: Option<bool>,
}

fn cmd_list(json: bool) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    warn_if_wayland_session();
    let windows = backend.list_windows()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&windows)?);
        return Ok(());
    }
    if windows.is_empty() {
        println!("no controllable windows");
        return Ok(());
    }
    println!(
        "{:<12} {:<20} {:>11} {:>13}  TITLE",
        "ID", "APP", "SIZE", "POSITION"
    );
    for window in &windows {
        let marker = if window.focused { "*" } else { " " };
        println!(
            "{}{:<11} {:<20} {:>11} {:>13}  {}",
            marker,
            window.id,
            truncate(window.app_id.as_deref().unwrap_or("-"), 20),
            format!("{}x{}", window.geometry.client.w, window.geometry.client.h),
            format!("{},{}", window.geometry.client.x, window.geometry.client.y),
            truncate(&window.title, 60),
        );
    }
    Ok(())
}

fn cmd_geometry(json: bool, window: &str, basis: SizeBasis) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, window)?;
    let geometry = backend.geometry(&id)?;
    let rect = geometry.rect(basis);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "window": id,
                "basis": basis,
                "rect": rect,
                "frame": geometry.frame,
                "client": geometry.client,
                "scale": geometry.scale,
                "decorated": geometry.decorated,
                "extents": geometry.extents(),
            }))?
        );
        return Ok(());
    }
    println!("window {}", id);
    println!(
        "{}   {}x{} at {},{}",
        basis_label(SizeBasis::Frame),
        geometry.frame.w,
        geometry.frame.h,
        geometry.frame.x,
        geometry.frame.y
    );
    println!(
        "{} {}x{} at {},{}",
        basis_label(SizeBasis::Client),
        geometry.client.w,
        geometry.client.h,
        geometry.client.x,
        geometry.client.y
    );
    let extents = geometry.extents();
    println!(
        "extents    left {} top {} right {} bottom {} ({})",
        extents.left,
        extents.top,
        extents.right,
        extents.bottom,
        if geometry.decorated {
            "decorated"
        } else {
            "undecorated"
        }
    );
    if basis == SizeBasis::Client {
        println!("selected   {} {}x{}", basis_label(basis), rect.w, rect.h);
    }
    Ok(())
}

fn cmd_resize(json: bool, request: ResizeRequest, config_path: Option<&Path>) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, &request.window)?;
    let area = backend.area(parse_monitor(&request.monitor)?, parse_area(&request.area)?)?;
    let spec = match (&request.size, &request.percent) {
        (Some(size), None) => {
            let (w, h) = parse_pixels(size)?;
            SizeSpec::Pixels(w, h)
        }
        (None, Some(percent)) => {
            let (w, h) = parse_percent(percent)?;
            SizeSpec::Percent(w, h)
        }
        _ => {
            return Err("resize needs exactly one of --size or --percent".into());
        }
    };
    let size = spec.resolve(area);
    let current = backend.geometry(&id)?.rect(request.basis);
    let anchor = position_or_anchor(&request.anchor, request.x, request.y)?;
    let origin = match anchor {
        Some(anchor) => anchor.place(area, size),
        None => current,
    };
    let target = Rect::new(origin.x, origin.y, size.0, size.1);
    let allow_outside = request
        .allow_outside
        .unwrap_or_else(|| configured_allow_outside(config_path));
    let result = backend.apply(&id, target, request.basis, area, allow_outside)?;
    report_apply(json, &id, &result, area)
}

/// The switch as configured. A missing or invalid file leaves the default (off)
/// instead of blocking a direct resize.
fn configured_allow_outside(explicit: Option<&Path>) -> bool {
    config_file_path(explicit)
        .ok()
        .and_then(|path| Config::load(&path).ok())
        .is_some_and(|config| config.options.allow_outside_bounds)
}

/// `--allow-outside` / `--no-allow-outside` override the configuration for one
/// command; `None` keeps whatever the config says.
fn override_switch(allow: bool, deny: bool) -> Option<bool> {
    if allow {
        Some(true)
    } else if deny {
        Some(false)
    } else {
        None
    }
}

/// `--anchor` and `--x/--y` choose the same point, so only one may be given.
fn position_or_anchor(
    anchor: &str,
    x: Option<i32>,
    y: Option<i32>,
) -> Result<Option<Anchor>, String> {
    if let Some((x, y)) = resolve_position(x, y)? {
        if !anchor.trim().is_empty() {
            return Err("--anchor and --x/--y are mutually exclusive".to_string());
        }
        return Ok(Some(Anchor::Custom(x, y)));
    }
    parse_anchor(anchor)
}

struct MoveRequest {
    anchor: String,
    x: Option<i32>,
    y: Option<i32>,
    window: String,
    monitor: String,
    area: String,
    basis: SizeBasis,
    allow_outside: Option<bool>,
}

fn cmd_move(json: bool, request: MoveRequest, config_path: Option<&Path>) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, &request.window)?;
    let area = backend.area(parse_monitor(&request.monitor)?, parse_area(&request.area)?)?;
    let current = backend.geometry(&id)?.rect(request.basis);
    let anchor = position_or_anchor(&request.anchor, request.x, request.y)?
        .ok_or("move needs --anchor or --x/--y")?;
    let target = anchor.place(area, (current.w, current.h));
    let allow_outside = request
        .allow_outside
        .unwrap_or_else(|| configured_allow_outside(config_path));
    let result = backend.apply(&id, target, request.basis, area, allow_outside)?;
    report_apply(json, &id, &result, area)
}

fn cmd_above(json: bool, window: &str, above: bool) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, window)?;
    backend.set_above(&id, above)?;
    let info = backend
        .list_windows()?
        .into_iter()
        .find(|info| info.id == id)
        .ok_or_else(|| BackendError::WindowNotFound(id.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"window": id, "above": info.above}))?
        );
    } else {
        println!("window {} above = {}", id, info.above);
    }
    Ok(())
}

fn cmd_state(json: bool, window: &str, maximize: bool) -> CliResult<()> {
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, window)?;
    let maximized = backend.set_maximized(&id, maximize)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"window": id, "maximized": maximized}))?
        );
    } else {
        println!("window {} maximized = {}", id, maximized);
    }
    Ok(())
}

fn cmd_hotkey(json: bool, accelerator: &str, verify_delivery: bool, hold: u64) -> CliResult<()> {
    if verify_delivery {
        eprintln!(
            "sizer: injecting one keystroke through XTEST to prove delivery; \
             do not use this on a session where synthetic input is unwanted"
        );
    }
    let policy = if verify_delivery {
        ProbePolicy::Inject
    } else {
        ProbePolicy::Off
    };
    let backend = X11Backend::connect_with(policy)?;
    let report = backend.register(&HotkeySpec {
        accelerator: accelerator.to_string(),
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        match &report.outcome {
            HotkeyOutcome::Verified => {
                println!(
                    "hotkey {} registered and delivery verified",
                    report.accelerator
                )
            }
            HotkeyOutcome::RegisteredUnverified { detail } => {
                println!(
                    "hotkey {} registered but unverified: {}",
                    report.accelerator, detail
                )
            }
            HotkeyOutcome::Conflict { detail } => {
                println!("hotkey {} unavailable: {}", report.accelerator, detail)
            }
        }
    }
    if hold > 0 {
        let pressed = backend.watch_keypresses(Duration::from_secs(hold))?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"keys": pressed}))?
            );
        } else {
            println!("fired {} time(s) in {}s", pressed.len(), hold);
        }
    }
    Ok(())
}

/// What the session bus says about the GNOME shell extension.
enum ShellStatus {
    Running { presets: usize, menu_open: bool },
    Unavailable(String),
}

impl ShellStatus {
    fn describe(&self) -> String {
        match self {
            ShellStatus::Running { presets, menu_open } => format!(
                "gnome extension running ({presets} presets, menu {})",
                if *menu_open { "open" } else { "closed" }
            ),
            ShellStatus::Unavailable(message) => message.clone(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            ShellStatus::Running { presets, menu_open } => {
                json!({"status": "running", "presets": presets, "menu_open": menu_open})
            }
            ShellStatus::Unavailable(message) => {
                json!({"status": "unavailable", "detail": message})
            }
        }
    }
}

fn probe_shell_extension() -> ShellStatus {
    let connection = match zbus::blocking::Connection::session() {
        Ok(connection) => connection,
        Err(error) => return ShellStatus::Unavailable(format!("session bus unavailable: {error}")),
    };
    let proxy = match zbus::blocking::Proxy::new(
        &connection,
        "org.sizerlinux.Shell",
        "/org/sizerlinux/Shell",
        "org.sizerlinux.Shell",
    ) {
        Ok(proxy) => proxy,
        Err(error) => return ShellStatus::Unavailable(error.to_string()),
    };
    match proxy.call::<_, _, String>("MenuState", &()) {
        Ok(reply) => {
            let state: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
            ShellStatus::Running {
                presets: state
                    .get("presets")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
                menu_open: state
                    .get("open")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            }
        }
        Err(_) => ShellStatus::Unavailable(
            "gnome extension not on the session bus (install it, log out and back in, \
             then `gnome-extensions enable sizer@sizerlinux.dev`)"
                .to_string(),
        ),
    }
}

/// One human-readable line plus the JSON form of the configuration status.
fn config_status(explicit: Option<&Path>) -> (String, serde_json::Value) {
    let Ok(path) = config_file_path(explicit) else {
        return (
            "cannot locate a configuration file (use --config, XDG_CONFIG_HOME or HOME)"
                .to_string(),
            json!({"status": "unresolved"}),
        );
    };
    if !path.exists() {
        return (
            format!("{} (missing; built-in presets apply)", path.display()),
            json!({"path": path, "status": "missing"}),
        );
    }
    match load_valid_config(&path) {
        Ok(config) => {
            let count = config.presets.len();
            (
                format!(
                    "{} ({count} preset{})",
                    path.display(),
                    if count == 1 { "" } else { "s" }
                ),
                json!({"path": path, "status": "ok", "presets": count}),
            )
        }
        Err(error) => {
            let message = error.to_string();
            let summary = message
                .lines()
                .next()
                .unwrap_or("configuration is invalid")
                .to_string();
            (
                format!(
                    "{} (invalid: {summary}; run `sizer config check`)",
                    path.display()
                ),
                json!({"path": path, "status": "invalid", "detail": message}),
            )
        }
    }
}

fn cmd_doctor(json: bool, explicit_config: Option<&Path>) -> CliResult<()> {
    let backend = X11Backend::connect();
    let wayland_session = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let session_line = format!(
        "type={} display={} wayland={}",
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into()),
        std::env::var("DISPLAY").unwrap_or_else(|_| "-".into()),
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "-".into()),
    );
    let session = json!({
        "xdg_session_type": std::env::var("XDG_SESSION_TYPE").ok(),
        "display": std::env::var("DISPLAY").ok(),
        "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
        // The X11 backend cannot see native Wayland windows.
        "xwayland_only": wayland_session,
    });
    let shell = probe_shell_extension();
    let (config_line, config_json) = config_status(explicit_config);
    let backend = match backend {
        Ok(backend) => backend,
        Err(err) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "session": session,
                        "config": config_json,
                        "shell_extension": shell.to_json(),
                        "backend_error": err.to_string(),
                    }))?
                );
            } else {
                println!("session  {session_line}");
                println!("config   {config_line}");
                println!("shell    {}", shell.describe());
            }
            return Err(err.into());
        }
    };
    let capabilities = backend.capabilities();
    let behavior = backend.behavior();
    let monitors = backend.monitors()?;
    let work_area = backend.area(MonitorSelector::Active, AreaKind::WorkArea)?;
    let active = backend.active_window().ok();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": session,
                "config": config_json,
                "shell_extension": shell.to_json(),
                "capabilities": capabilities,
                "behavior": behavior,
                "monitors": monitors,
                "active_work_area": work_area,
                "active_window": active,
            }))?
        );
        return Ok(());
    }
    println!("session  {session_line}");
    println!("config   {config_line}");
    println!("shell    {}", shell.describe());
    println!(
        "backend  {} ({})",
        capabilities.backend,
        geometry_write_label(behavior)
    );
    println!(
        "caps     geometry={} above={} decorations={} hotkeys={} conflict_detectable={} delivery_probe={}",
        capabilities.geometry,
        capabilities.stay_above,
        capabilities.decorations,
        capabilities.hotkeys,
        capabilities.hotkey_conflict_detectable,
        capabilities.hotkey_delivery_probe,
    );
    println!("monitors {}", monitors.len());
    for (index, monitor) in monitors.iter().enumerate() {
        println!("  [{}] {}", index, describe_monitor(monitor));
    }
    println!(
        "workarea {}x{} at {},{}",
        work_area.w, work_area.h, work_area.x, work_area.y
    );
    match active {
        Some(id) => println!("active   {id}"),
        None => println!("active   none"),
    }
    warn_if_wayland_session();
    Ok(())
}

/// A missing file starts empty instead of inheriting the built-in presets:
/// writing those back would silently turn defaults into user configuration.
fn load_editable_config(path: &Path) -> CliResult<Config> {
    if path.exists() {
        load_valid_config(path)
    } else {
        Ok(Config::default())
    }
}

/// Group identity for ordering and duplicate checks, insensitive to the
/// spelling differences (`A/B`, `A / B`, ` / `) that render as the same menu.
fn preset_group_key(preset: &Preset) -> String {
    sizer_config::menu::group_path(preset.group.as_deref())
        .join("\u{1f}")
        .to_ascii_lowercase()
}

/// Name for `preset copy`: "<label> copy", then " copy 2", ... within the group.
fn copy_label(config: &Config, source: &str, group: Option<&str>) -> String {
    let key = sizer_config::menu::group_path(group)
        .join("\u{1f}")
        .to_ascii_lowercase();
    let taken = |candidate: &str| {
        config.presets.iter().any(|preset| {
            !preset.is_separator()
                && preset_group_key(preset) == key
                && preset.label.eq_ignore_ascii_case(candidate)
        })
    };
    let base = format!("{} copy", source.trim());
    if !taken(&base) {
        return base;
    }
    let mut counter = 2;
    loop {
        let candidate = format!("{base} {counter}");
        if !taken(&candidate) {
            return candidate;
        }
        counter += 1;
    }
}

fn describe_group(preset: &Preset) -> String {
    match preset.group.as_deref() {
        Some(group) => format!("group {group:?}"),
        None => "the top level".to_string(),
    }
}

fn check_preset(preset: &Preset) -> Result<(), String> {
    if preset.is_separator() {
        return Ok(());
    }
    if preset.label.trim().is_empty() {
        return Err("preset label must not be empty".to_string());
    }
    if preset.size.trim().is_empty() {
        // The move-only form (Sizer's `<relocate>`): it keeps the current size.
        if preset.position().is_none() && preset.anchor.is_none() {
            return Err("a preset needs a size unless it names x/y or an anchor".to_string());
        }
    } else {
        preset.size_spec()?;
    }
    preset.monitor_selector()?;
    if let Some(message) = preset
        .hotkey
        .as_deref()
        .and_then(|hotkey| sizer_backend_x11::validate_accelerator(hotkey).err())
    {
        return Err(format!("hotkey: {message}"));
    }
    Ok(())
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Size string for `sizer preset save`, captured in the basis `sizer apply`
/// will interpret it with.
///
/// Saving the client size while applying it as a frame (or the other way round)
/// moves the window a decoration-width on every apply, so the capture and the
/// interpretation must share one basis.
fn saved_size(geometry: &Geometry, basis: SizeBasis) -> String {
    let rect = geometry.rect(basis);
    format!("{}x{}", rect.w, rect.h)
}

#[allow(clippy::too_many_arguments)] // one field per CLI flag keeps call sites readable
fn build_preset(
    label: &str,
    size: Option<&str>,
    group: Option<&str>,
    anchor: Option<&str>,
    hotkey: Option<&str>,
    monitor: Option<&str>,
    x: Option<i32>,
    y: Option<i32>,
) -> CliResult<Preset> {
    let anchor = parse_anchor(anchor.unwrap_or(""))?;
    let position = resolve_position(x, y)?;
    if position.is_some() && anchor.is_some() {
        return Err("--anchor and --x/--y are mutually exclusive".into());
    }
    let preset = Preset {
        label: label.trim().to_string(),
        size: size.unwrap_or("").trim().to_string(),
        group: sizer_config::menu::normalize_group(group),
        anchor,
        hotkey: normalize_optional(hotkey),
        monitor: normalize_optional(monitor),
        x: position.map(|position| position.0),
        y: position.map(|position| position.1),
        allow_outside_bounds: None,
        kind: sizer_config::PresetKind::Resize,
    };
    check_preset(&preset)?;
    Ok(preset)
}

/// An absolute position is a pair: half of one would place the window against
/// an unintended edge.
fn resolve_position(x: Option<i32>, y: Option<i32>) -> Result<Option<(i32, i32)>, String> {
    match (x, y) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) => Ok(Some((x, y))),
        (Some(_), None) => Err("--x needs --y as well".to_string()),
        (None, Some(_)) => Err("--y needs --x as well".to_string()),
    }
}

fn ensure_unique(config: &Config, preset: &Preset, skip: Option<usize>) -> CliResult<()> {
    if preset.is_separator() {
        return Ok(());
    }
    let group = preset_group_key(preset);
    let duplicate = config.presets.iter().enumerate().find(|(index, other)| {
        Some(*index) != skip
            && !other.is_separator()
            && preset_group_key(other) == group
            && other.label.trim().eq_ignore_ascii_case(preset.label.trim())
    });
    if let Some((_, existing)) = duplicate {
        return Err(format!(
            "preset {:?} already exists in {}; use `sizer preset set` to change it",
            existing.label,
            describe_group(existing)
        )
        .into());
    }
    Ok(())
}

/// Reorder one preset to a 1-based position inside its own group, which is
/// the order the menu actually shows.
fn reorder_preset(config: &mut Config, from: usize, position: usize) -> CliResult<()> {
    if position == 0 {
        return Err("--position is 1-based and must be at least 1".into());
    }
    let preset = config.presets.remove(from);
    let group = preset_group_key(&preset);
    let same_group: Vec<usize> = config
        .presets
        .iter()
        .enumerate()
        .filter(|(_, other)| preset_group_key(other) == group)
        .map(|(index, _)| index)
        .collect();
    if position > same_group.len() + 1 {
        return Err(format!(
            "--position {position} is out of range: {} holds {} preset(s)",
            describe_group(&preset),
            same_group.len() + 1
        )
        .into());
    }
    let insert_at = if position <= same_group.len() {
        same_group[position - 1]
    } else {
        same_group
            .last()
            .map(|index| index + 1)
            .unwrap_or(config.presets.len())
    };
    config.presets.insert(insert_at, preset);
    Ok(())
}

/// Move a preset (after its group changed) to the end of its new group.
fn place_last_in_group(config: &mut Config, from: usize) {
    let preset = config.presets.remove(from);
    let group = preset_group_key(&preset);
    let insert_at = config
        .presets
        .iter()
        .rposition(|other| preset_group_key(other) == group)
        .map(|index| index + 1)
        .unwrap_or(config.presets.len());
    config.presets.insert(insert_at, preset);
}

/// Validate the whole file before touching the disk, then use the atomic save.
fn save_presets(path: &Path, config: &Config) -> CliResult<()> {
    let mut problems = config.validate();
    problems.extend(
        config
            .options
            .hotkey
            .as_deref()
            .and_then(|hotkey| hotkey_problem("options.hotkey".to_string(), hotkey)),
    );
    for (index, preset) in config.presets.iter().enumerate() {
        problems.extend(
            preset
                .hotkey
                .as_deref()
                .and_then(|hotkey| hotkey_problem(format!("presets[{index}].hotkey"), hotkey)),
        );
    }
    if !problems.is_empty() {
        return Err(Box::new(ConfigError::Invalid(problems)));
    }
    config.save(path)?;
    Ok(())
}

fn anchor_text(anchor: Anchor) -> String {
    serde_json::to_value(anchor)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{anchor:?}"))
}

fn report_preset(json: bool, action: &str, path: &Path, preset: &Preset) -> CliResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "action": action,
                "path": path,
                "preset": preset,
            }))?
        );
    } else if preset.is_separator() {
        println!(
            "{action} separator in {}",
            preset
                .group
                .as_deref()
                .map(|group| format!("group {group:?}"))
                .unwrap_or_else(|| "the top level".to_string()),
        );
    } else {
        println!(
            "{action} {:?} ({}) in {}",
            preset_label(preset),
            preset.size,
            path.display()
        );
    }
    Ok(())
}

fn cmd_preset(json: bool, action: PresetAction, explicit_config: Option<&Path>) -> CliResult<()> {
    let path = config_file_path(explicit_config)?;
    match action {
        PresetAction::List => {
            let config = if path.exists() {
                load_valid_config(&path)?
            } else {
                Config::default()
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"path": path, "presets": config.presets})
                    )?
                );
            } else {
                println!("IDX | GROUP | LABEL | SIZE | ANCHOR | HOTKEY");
                for (index, preset) in config.presets.iter().enumerate() {
                    if preset.is_separator() {
                        println!(
                            "{} | {} | <separator> | - | - | -",
                            index + 1,
                            preset.group.as_deref().unwrap_or("-"),
                        );
                        continue;
                    }
                    println!(
                        "{} | {} | {} | {} | {} | {}",
                        index + 1,
                        preset.group.as_deref().unwrap_or("-"),
                        preset.label,
                        preset.size,
                        preset
                            .anchor
                            .map(anchor_text)
                            .unwrap_or_else(|| "-".to_string()),
                        preset.hotkey.as_deref().unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        PresetAction::Add {
            label,
            size,
            group,
            anchor,
            hotkey,
            monitor,
            x,
            y,
            position,
        } => {
            let mut config = load_editable_config(&path)?;
            let preset = build_preset(
                &label,
                size.as_deref(),
                group.as_deref(),
                anchor.as_deref(),
                hotkey.as_deref(),
                monitor.as_deref(),
                x,
                y,
            )?;
            ensure_unique(&config, &preset, None)?;
            let index = config.presets.len();
            config.presets.push(preset.clone());
            if let Some(position) = position {
                reorder_preset(&mut config, index, position)?;
            }
            save_presets(&path, &config)?;
            report_preset(json, "added", &path, &preset)
        }
        PresetAction::Set {
            selector,
            label,
            size,
            group,
            anchor,
            hotkey,
            monitor,
            x,
            y,
            no_position,
        } => {
            let mut config = load_valid_config(&path)?;
            let index = find_preset_index(&config, &selector)?;
            ensure_not_separator(&config, index, "changing")?;
            let old_group = preset_group_key(&config.presets[index]);
            let mut preset = config.presets[index].clone();
            if let Some(label) = label {
                preset.label = label.trim().to_string();
            }
            if let Some(size) = size {
                preset.size = size.trim().to_string();
            }
            if let Some(group) = group {
                preset.group = sizer_config::menu::normalize_group(Some(&group));
            }
            if no_position {
                preset.x = None;
                preset.y = None;
            }
            if let Some(position) = resolve_position(x, y)? {
                preset.x = Some(position.0);
                preset.y = Some(position.1);
                // A position and an anchor name the same point in two ways.
                preset.anchor = None;
            }
            if let Some(anchor) = anchor {
                preset.anchor = parse_anchor(&anchor)?;
                preset.x = None;
                preset.y = None;
            }
            if let Some(hotkey) = hotkey {
                preset.hotkey = normalize_optional(Some(&hotkey));
            }
            if let Some(monitor) = monitor {
                preset.monitor = normalize_optional(Some(&monitor));
            }
            check_preset(&preset)?;
            ensure_unique(&config, &preset, Some(index))?;
            let group_changed = preset_group_key(&preset) != old_group;
            config.presets[index] = preset.clone();
            if group_changed {
                place_last_in_group(&mut config, index);
            }
            save_presets(&path, &config)?;
            report_preset(json, "updated", &path, &preset)
        }
        PresetAction::Remove { selector } => {
            let mut config = load_valid_config(&path)?;
            let index = find_preset_index(&config, &selector)?;
            let removed = config.presets.remove(index);
            save_presets(&path, &config)?;
            report_preset(json, "removed", &path, &removed)
        }
        PresetAction::Move { selector, position } => {
            let mut config = load_valid_config(&path)?;
            let index = find_preset_index(&config, &selector)?;
            let preset = config.presets[index].clone();
            reorder_preset(&mut config, index, position)?;
            save_presets(&path, &config)?;
            report_preset(json, "moved", &path, &preset)
        }
        PresetAction::Copy {
            selector,
            label,
            group,
            x,
            y,
            position,
        } => {
            let mut config = load_editable_config(&path)?;
            let index = find_preset_index(&config, &selector)?;
            ensure_not_separator(&config, index, "copying")?;
            let source = config.presets[index].clone();
            let group = match group {
                Some(group) => sizer_config::menu::normalize_group(Some(&group)),
                None => source.group.clone(),
            };
            let label = match label {
                Some(label) => label.trim().to_string(),
                None => copy_label(&config, &source.label, group.as_deref()),
            };
            let position_override = resolve_position(x, y)?;
            let (anchor, x, y) = match position_override {
                Some((x, y)) => (None, Some(x), Some(y)),
                None => (source.anchor, source.x, source.y),
            };
            let preset = Preset {
                label,
                size: source.size,
                group,
                anchor,
                hotkey: None,
                monitor: source.monitor,
                x,
                y,
                allow_outside_bounds: None,
                kind: sizer_config::PresetKind::Resize,
            };
            check_preset(&preset)?;
            ensure_unique(&config, &preset, None)?;
            let index = config.presets.len();
            config.presets.push(preset.clone());
            if let Some(position) = position {
                reorder_preset(&mut config, index, position)?;
            }
            save_presets(&path, &config)?;
            report_preset(json, "copied", &path, &preset)
        }
        PresetAction::Separator { group, position } => {
            let mut config = load_editable_config(&path)?;
            let preset = Preset::separator(sizer_config::menu::normalize_group(group.as_deref()));
            let index = config.presets.len();
            config.presets.push(preset.clone());
            if let Some(position) = position {
                reorder_preset(&mut config, index, position)?;
            }
            save_presets(&path, &config)?;
            report_preset(json, "separator", &path, &preset)
        }
        PresetAction::Save {
            label,
            group,
            window,
        } => {
            let mut config = load_editable_config(&path)?;
            let backend = X11Backend::connect()?;
            let id = resolve(&backend, &window)?;
            let geometry = backend.geometry(&id)?;
            let size = saved_size(&geometry, config.options.basis);
            let preset = build_preset(
                &label,
                Some(size.as_str()),
                group.as_deref(),
                None,
                None,
                None,
                None,
                None,
            )?;
            ensure_unique(&config, &preset, None)?;
            config.presets.push(preset.clone());
            save_presets(&path, &config)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "action": "saved",
                        "path": path,
                        "window": id,
                        "size": size,
                        "preset": preset,
                    }))?
                );
            } else {
                println!(
                    "saved {size} from window {id} as {:?} in {}",
                    preset_label(&preset),
                    path.display()
                );
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the flag set of `sizer apply`
fn cmd_apply(
    json: bool,
    label: &str,
    window: &str,
    monitor: Option<&str>,
    area: Option<&str>,
    basis: Option<&str>,
    allow_outside: Option<bool>,
    explicit_config: Option<&Path>,
) -> CliResult<()> {
    let config = load_valid_config(&config_file_path(explicit_config)?)?;
    let preset = find_preset(&config, label)?;
    let backend = X11Backend::connect()?;
    let id = resolve(&backend, window)?;
    let monitor = match monitor {
        Some(text) => parse_monitor(text)?,
        None => preset
            .monitor_selector()
            .map_err(|message| -> Box<dyn Error> { message.into() })?,
    };
    let area_kind = match area {
        Some(text) => parse_area(text)?,
        None => config.options.area,
    };
    let basis = match basis {
        Some(text) => parse_basis(text)?,
        None => config.options.basis,
    };
    let allow_outside =
        allow_outside.unwrap_or_else(|| preset.allows_outside(config.options.allow_outside_bounds));
    let result = apply_preset(
        &backend,
        &id,
        preset,
        monitor,
        area_kind,
        basis,
        allow_outside,
    )?;
    let area_rect = backend.area(monitor, area_kind)?;
    report_apply(json, &id, &result, area_rect)
}

/// Resolve a preset's size and anchor against the reference area and apply it.
fn apply_preset(
    backend: &X11Backend,
    id: &WindowId,
    preset: &Preset,
    monitor: MonitorSelector,
    area_kind: AreaKind,
    basis: SizeBasis,
    allow_outside: bool,
) -> CliResult<GeometryResult> {
    let area = backend.area(monitor, area_kind)?;
    let current = backend.geometry(id)?.rect(basis);
    // A move-only preset keeps the size the window has right now.
    let size = if preset.is_move_only() {
        (current.w, current.h)
    } else {
        preset
            .size_spec()
            .map_err(|message| -> Box<dyn Error> { message.into() })?
            .resolve(area)
    };
    let origin = preset.place(area, size).unwrap_or(current);
    let target = Rect::new(origin.x, origin.y, size.0, size.1);
    Ok(backend.apply(id, target, basis, area, allow_outside)?)
}

/// `preset set`/`copy` need a real preset; a separator only has a position.
fn ensure_not_separator(config: &Config, index: usize, action: &str) -> CliResult<()> {
    if config.presets[index].is_separator() {
        return Err(format!(
            "row #{} is a menu separator; {action} it makes no sense — remove it instead",
            index + 1
        )
        .into());
    }
    Ok(())
}

/// Look a preset up by label, or by `group / label` when labels repeat.
///
/// `#N` selects the row with that 1-based `IDX` from `sizer preset list`, which
/// is the only way to address a separator.
fn find_preset_index(config: &Config, selector: &str) -> CliResult<usize> {
    let trimmed = selector.trim();
    if let Some(number) = trimmed.strip_prefix('#') {
        let index: usize = number.parse().map_err(|_| {
            format!("row {trimmed:?} is not a number; use #IDX with the IDX column of `sizer preset list`")
        })?;
        if index == 0 || index > config.presets.len() {
            return Err(format!(
                "row #{index} is out of range: the configuration holds {} entr{}",
                config.presets.len(),
                if config.presets.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            )
            .into());
        }
        return Ok(index - 1);
    }
    let wanted = trimmed.to_ascii_lowercase();
    let matches: Vec<usize> = config
        .presets
        .iter()
        .enumerate()
        .filter(|(_, preset)| {
            !preset.is_separator()
                && (preset.label.eq_ignore_ascii_case(&wanted)
                    || preset_label(preset).to_ascii_lowercase() == wanted)
        })
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [single] => Ok(*single),
        [] => {
            let available: Vec<String> = config
                .presets
                .iter()
                .enumerate()
                .map(|(index, preset)| {
                    if preset.is_separator() {
                        format!("<separator> (#{})", index + 1)
                    } else {
                        preset_label(preset)
                    }
                })
                .collect();
            Err(format!(
                "no preset {selector:?}; available: {}",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )
            .into())
        }
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|index| preset_label(&config.presets[*index]))
                .collect();
            Err(format!(
                "preset {selector:?} is ambiguous; qualify it with the group: {}",
                candidates.join(", ")
            )
            .into())
        }
    }
}

fn find_preset<'a>(config: &'a Config, label: &str) -> CliResult<&'a Preset> {
    Ok(&config.presets[find_preset_index(config, label)?])
}

fn preset_label(preset: &Preset) -> String {
    match preset.group.as_deref().filter(|group| !group.is_empty()) {
        Some(group) => format!("{group} / {}", preset.label),
        None => preset.label.clone(),
    }
}

/// Preset behind one key event, matching both keycode and modifiers.
///
/// Two hotkeys may share a keycode (`<Super><Alt>F8` and `<Ctrl><Alt>F8`), so a
/// keycode-only lookup would apply whichever was registered first.
fn preset_for_key<'a>(
    keymap: &[(u8, u16, &'a Preset)],
    keycode: u8,
    state: u16,
) -> Option<&'a Preset> {
    keymap
        .iter()
        .find(|(code, mask, _)| *code == keycode && *mask == state)
        .map(|(_, _, preset)| *preset)
}

fn cmd_hotkeys(
    json: bool,
    verify_delivery: bool,
    hold: u64,
    window: &str,
    explicit_config: Option<&Path>,
) -> CliResult<()> {
    if verify_delivery {
        eprintln!(
            "sizer: injecting one keystroke per hotkey through XTEST to prove delivery; \
             do not use this on a session where synthetic input is unwanted"
        );
    }
    let config = load_menu_config(explicit_config)?;
    let policy = if verify_delivery {
        ProbePolicy::Inject
    } else {
        ProbePolicy::Off
    };
    let backend = X11Backend::connect_with(policy)?;

    let mut keymap: Vec<(u8, u16, &Preset)> = Vec::new();
    let mut entries = Vec::new();
    let mut conflicts = 0usize;
    for preset in &config.presets {
        let Some(accelerator) = preset
            .hotkey
            .as_deref()
            .filter(|key| !key.trim().is_empty())
        else {
            continue;
        };
        let report = backend.register(&HotkeySpec {
            accelerator: accelerator.to_string(),
        })?;
        if matches!(report.outcome, HotkeyOutcome::Conflict { .. }) {
            conflicts += 1;
        } else if let Ok((keycode, mask)) = backend.keycode_and_mask_for(accelerator) {
            keymap.push((keycode, mask, preset));
        }
        entries.push((preset_label(preset), report));
    }
    let mut menu_key = None;
    if let Some(accelerator) = config.options.hotkey.clone() {
        let report = backend.register(&HotkeySpec {
            accelerator: accelerator.clone(),
        })?;
        if matches!(report.outcome, HotkeyOutcome::Conflict { .. }) {
            conflicts += 1;
        } else if let Ok((keycode, mask)) = backend.keycode_and_mask_for(&accelerator) {
            menu_key = Some((keycode, mask));
        }
        entries.push(("menu".to_string(), report));
    }

    if json {
        let payload: Vec<_> = entries
            .iter()
            .map(|(preset, report)| json!({"preset": preset, "report": report}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if entries.is_empty() {
        println!("no hotkeys configured");
    } else {
        for (preset, report) in &entries {
            match &report.outcome {
                HotkeyOutcome::Verified => {
                    println!(
                        "{} -> {preset}: registered and delivery verified",
                        report.accelerator
                    )
                }
                HotkeyOutcome::RegisteredUnverified { detail } => println!(
                    "{} -> {preset}: registered but unverified: {detail}",
                    report.accelerator
                ),
                HotkeyOutcome::Conflict { detail } => {
                    println!("{} -> {preset}: unavailable: {detail}", report.accelerator)
                }
            }
        }
    }
    if entries.is_empty() {
        if hold > 0 {
            return Err(
                "no hotkeys configured; add `hotkey = ...` to a preset or options.hotkey".into(),
            );
        }
        return Ok(());
    }
    if conflicts > 0 {
        return Err(format!(
            "{conflicts} configured hotkey(s) are already owned by another application"
        )
        .into());
    }

    if hold > 0 {
        let deadline = Instant::now() + Duration::from_secs(hold);
        while Instant::now() < deadline {
            let Some((keycode, state)) =
                backend.next_keypress(deadline.saturating_duration_since(Instant::now()))?
            else {
                break;
            };
            // Two hotkeys can differ only in modifiers; compare both halves.
            let state = sizer_backend_x11::hotkey_modifiers(state);
            if Some((keycode, state)) == menu_key {
                match open_menu(&backend, &config, window)? {
                    Some(outcome) => {
                        report_apply(json, &outcome.window, &outcome.result, outcome.area)?
                    }
                    None if !json => println!("menu closed without a selection"),
                    None => {}
                }
                continue;
            }
            let Some(preset) = preset_for_key(&keymap, keycode, state) else {
                continue;
            };
            let id = resolve(&backend, window)?;
            let monitor = preset
                .monitor_selector()
                .map_err(|message| -> Box<dyn Error> { message.into() })?;
            let result = apply_preset(
                &backend,
                &id,
                preset,
                monitor,
                config.options.area,
                config.options.basis,
                preset.allows_outside(config.options.allow_outside_bounds),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"preset": preset_label(preset), "window": id, "result": result})
                    )?
                );
            } else {
                let (dx, dy, dw, dh) = result.delta();
                println!(
                    "{} applied to {id}: {} {}x{} delta {dx},{dy},{dw},{dh}",
                    preset_label(preset),
                    basis_label(result.basis),
                    result.achieved.w,
                    result.achieved.h
                );
            }
        }
    }
    Ok(())
}

/// A preset chosen from the cursor menu, already applied.
struct MenuOutcome {
    window: WindowId,
    result: GeometryResult,
    area: Rect,
}

/// `sizer menu`: the entry point the desktop file runs.
///
/// GNOME's shell keeps the pointer and keyboard grabbed, so on GNOME the menu
/// lives in the shell extension; an X11 session without the extension gets the
/// cursor menu. A desktop entry has no terminal, so when neither can open a
/// menu the reason is shown as a desktop notification and the exit code is
/// non-zero instead of leaving the user with a no-op.
fn cmd_menu(json: bool, explicit_config: Option<&Path>) -> CliResult<()> {
    let mut extension_failure = None;
    match extension_show_menu() {
        Ok(reply) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&reply).unwrap_or_else(|_| json!({"raw": reply}));
            if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                if json {
                    println!("{reply}");
                } else {
                    println!("menu opened by the GNOME shell extension");
                }
                return Ok(());
            }
            eprintln!("sizer: shell extension did not open the menu: {reply}");
        }
        Err(failure) => {
            eprintln!("sizer: {}", failure.describe());
            extension_failure = Some(failure);
        }
    }
    match cmd_popup(json, "active", explicit_config) {
        Ok(()) => Ok(()),
        Err(error) => {
            notify_menu_failure(&error.to_string(), extension_failure.as_ref());
            Err(error)
        }
    }
}

/// Why the shell extension did not open its menu.
///
/// Telling these apart matters: "not installed" and "installed but too old"
/// need different advice, and neither is the same as a call that failed.
#[derive(Debug, PartialEq, Eq)]
enum ExtensionFailure {
    /// No process owns the extension's bus name.
    NotRunning,
    /// The extension answers but does not offer `ShowMenu`.
    TooOld,
    /// The extension was reached and the call failed, with the original detail.
    CallFailed(String),
}

impl ExtensionFailure {
    /// What the user can do about it, phrased for both the terminal and the
    /// desktop notification that a launcher shows instead of stderr.
    fn advice(&self) -> String {
        match self {
            ExtensionFailure::NotRunning => "the GNOME shell extension is not running in this \
                session; enable it with `gnome-extensions enable sizer@sizerlinux.dev`"
                .to_string(),
            ExtensionFailure::TooOld => "the installed GNOME shell extension has no ShowMenu \
                method (it is older than this sizer); reinstall the package and log in again"
                .to_string(),
            ExtensionFailure::CallFailed(detail) => {
                format!("the GNOME shell extension could not open the menu: {detail}")
            }
        }
    }

    /// The same advice for stderr, where it is followed by the fallback attempt.
    fn describe(&self) -> String {
        format!("{}; trying the cursor menu", self.advice())
    }
}

/// Body of the notification shown when neither menu path worked.
///
/// A launcher has no terminal, so this text is all the user sees: it must name
/// the actual cause instead of repeating one generic "install the extension".
fn menu_failure_notification(detail: &str, failure: Option<&ExtensionFailure>) -> String {
    match failure {
        Some(failure) => format!("{detail}\n{}", failure.advice()),
        None => format!("{detail}\nCtrl+Win+Z opens the menu when the shell extension is enabled."),
    }
}

/// Ask the shell extension to open its menu at the pointer, if it is running.
fn extension_show_menu() -> Result<String, ExtensionFailure> {
    let connection = zbus::blocking::Connection::session()
        .map_err(|error| ExtensionFailure::CallFailed(format!("no session bus: {error}")))?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.sizerlinux.Shell",
        "/org/sizerlinux/Shell",
        "org.sizerlinux.Shell",
    )
    .map_err(|error| ExtensionFailure::CallFailed(error.to_string()))?;
    // A negative coordinate asks the extension for the current pointer position,
    // so this works without an X connection of our own.
    proxy
        .call::<_, _, String>("ShowMenu", &(-1i32, -1i32))
        .map_err(|error| match &error {
            zbus::Error::MethodError(name, _, _) => {
                classify_extension_failure(name.as_str(), &error.to_string())
            }
            _ => ExtensionFailure::CallFailed(error.to_string()),
        })
}

/// Map a D-Bus error name onto the three cases the CLI can act on.
fn classify_extension_failure(name: &str, detail: &str) -> ExtensionFailure {
    match name.rsplit('.').next().unwrap_or(name) {
        "ServiceUnknown" | "NameHasNoOwner" => ExtensionFailure::NotRunning,
        "UnknownMethod" | "UnknownInterface" => ExtensionFailure::TooOld,
        _ => ExtensionFailure::CallFailed(detail.to_string()),
    }
}

/// Show why no menu appeared through the desktop notification service.
///
/// The caller still returns the error, so a terminal invocation also gets the
/// message on stderr; this exists because the desktop entry has no terminal.
fn notify_menu_failure(detail: &str, failure: Option<&ExtensionFailure>) {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    ) else {
        return;
    };
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    let actions: Vec<&str> = Vec::new();
    let body = menu_failure_notification(detail, failure);
    if let Err(error) = proxy.call::<_, _, u32>(
        "Notify",
        &(
            "Sizer",
            0u32,
            "",
            "Sizer could not open its menu",
            body,
            actions,
            hints,
            8000i32,
        ),
    ) {
        eprintln!("sizer: desktop notification failed: {error}");
    }
}

fn cmd_popup(json: bool, window: &str, explicit_config: Option<&Path>) -> CliResult<()> {
    warn_if_wayland_session();
    let config = load_menu_config(explicit_config)?;
    if config.presets.is_empty() {
        return Err("no presets configured; the menu would be empty".into());
    }
    let backend = X11Backend::connect()?;
    match open_menu(&backend, &config, window)? {
        Some(outcome) => report_apply(json, &outcome.window, &outcome.result, outcome.area),
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"selected": null}))?
                );
            } else {
                println!("menu closed without a selection");
            }
            Ok(())
        }
    }
}

/// Show the preset menu at the pointer and apply the chosen preset.
///
/// The target window is resolved after the menu closes: the menu is
/// override-redirect and never takes focus, so this keeps the target exactly the
/// window the user intended, and a cancelled menu touches no window at all.
fn open_menu(
    backend: &X11Backend,
    config: &Config,
    window: &str,
) -> CliResult<Option<MenuOutcome>> {
    let pointer = backend.pointer()?;
    let placement = backend.area_at(pointer, config.options.area)?;
    let menu = sizer_config::menu::menu(&config.presets);
    let Some(index) = sizer_popup::show_menu(&menu, pointer, placement)? else {
        return Ok(None);
    };
    let preset = &config.presets[index];
    let monitor = preset
        .monitor_selector()
        .map_err(|message| -> Box<dyn Error> { message.into() })?;
    let id = resolve(backend, window)?;
    let result = apply_preset(
        backend,
        &id,
        preset,
        monitor,
        config.options.area,
        config.options.basis,
        preset.allows_outside(config.options.allow_outside_bounds),
    )?;
    let area = backend.area(monitor, config.options.area)?;
    Ok(Some(MenuOutcome {
        window: id,
        result,
        area,
    }))
}

fn cmd_tray(json: bool, seconds: u64, explicit_config: Option<&Path>) -> CliResult<()> {
    let config = load_menu_config(explicit_config)?;
    let tray = sizer_tray::TrayConfig::from_config(&config)?;
    let _handle = sizer_tray::spawn(tray)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "tray": "sizer",
                "presets": config.presets.len(),
                "pid": std::process::id(),
            }))?
        );
    } else {
        println!(
            "tray registered ({} presets); the menu is published over StatusNotifierItem",
            config.presets.len()
        );
    }
    if seconds > 0 {
        std::thread::sleep(Duration::from_secs(seconds));
    } else {
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

fn cmd_config(json: bool, action: ConfigAction, explicit_config: Option<&Path>) -> CliResult<()> {
    match action {
        ConfigAction::Path => {
            let path = config_file_path(explicit_config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&json!({"path": path}))?);
            } else {
                println!("{}", path.display());
            }
            Ok(())
        }
        ConfigAction::Edit => {
            if json {
                return Err("config edit opens a window; drop --json".into());
            }
            let path = config_file_path(explicit_config)?;
            configure::run(&path)
        }
        ConfigAction::Check => {
            let path = config_file_path(explicit_config)?;
            let config = load_config(&path)?;
            let mut problems = config.validate();
            problems.extend(
                config
                    .options
                    .hotkey
                    .as_deref()
                    .and_then(|hotkey| hotkey_problem("options.hotkey".to_string(), hotkey)),
            );
            for (index, preset) in config.presets.iter().enumerate() {
                problems.extend(
                    preset.hotkey.as_deref().and_then(|hotkey| {
                        hotkey_problem(format!("presets[{index}].hotkey"), hotkey)
                    }),
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"path": path, "presets": config.presets.len(), "problems": problems})
                    )?
                );
            } else if problems.is_empty() {
                println!("{} ok ({} presets)", path.display(), config.presets.len());
            }
            if problems.is_empty() {
                Ok(())
            } else {
                Err(Box::new(ConfigError::Invalid(problems)))
            }
        }
        ConfigAction::Show => {
            let path = config_file_path(explicit_config)?;
            let config = load_config(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&config)?);
            } else {
                print!("{}", config.to_toml()?);
            }
            Ok(())
        }
        ConfigAction::Import { xml, write } => {
            let text = std::fs::read_to_string(&xml)
                .map_err(|error| format!("{}: {error}", xml.display()))?;
            let report = sizer_xml::import(&text)?;
            for item in &report.skipped {
                eprintln!(
                    "sizer: skipped <{}> {:?}: {}",
                    item.element, item.label, item.reason
                );
            }
            let problems = report.config.validate();
            if !problems.is_empty() {
                return Err(Box::new(ConfigError::Invalid(problems)));
            }
            if write {
                let path = config_file_path(explicit_config)?;
                report.config.save(&path)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "path": path,
                            "presets": report.config.presets.len(),
                            "skipped": report.skipped,
                        }))?
                    );
                } else {
                    println!(
                        "wrote {} ({} presets, {} skipped)",
                        path.display(),
                        report.config.presets.len(),
                        report.skipped.len()
                    );
                }
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"config": report.config, "skipped": report.skipped})
                    )?
                );
            } else {
                print!("{}", report.config.to_toml()?);
            }
            Ok(())
        }
        ConfigAction::Export { out } => {
            let config = load_config(&config_file_path(explicit_config)?)?;
            let xml = sizer_xml::export(&config)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &xml)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &json!({"path": path, "bytes": xml.len()})
                            )?
                        );
                    } else {
                        println!("wrote {}", path.display());
                    }
                }
                None => print!("{xml}"),
            }
            Ok(())
        }
    }
}

fn config_file_path(explicit: Option<&Path>) -> CliResult<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    Ok(Config::default_path()
        .ok_or("cannot locate the configuration file: set XDG_CONFIG_HOME or HOME")?)
}

fn load_config(path: &Path) -> CliResult<Config> {
    if !path.exists() {
        return Err(format!(
            "no configuration file at {} (create it with `sizer config import <sizer.xml> --write`)",
            path.display()
        )
        .into());
    }
    Ok(Config::load(path)?)
}

/// Load and reject a configuration that parses but cannot be used as written.
fn load_valid_config(path: &Path) -> CliResult<Config> {
    let config = load_config(path)?;
    let problems = config.validate();
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(Box::new(ConfigError::Invalid(problems)))
    }
}

/// One invalid accelerator, pointing at the field that contains it.
fn hotkey_problem(field: String, accelerator: &str) -> Option<Problem> {
    sizer_backend_x11::validate_accelerator(accelerator)
        .err()
        .map(|message| Problem { field, message })
}

/// Configuration for the menu entry points (`popup`, `hotkeys`, `tray`).
///
/// Those commands are how a first run works, so a missing file falls back to
/// the built-in preset list instead of failing. An existing file is validated:
/// silently ignoring a broken one would hide the user's real presets.
fn load_menu_config(explicit: Option<&Path>) -> CliResult<Config> {
    let Ok(path) = config_file_path(explicit) else {
        eprintln!("sizer: cannot locate a configuration file; using built-in presets");
        return Ok(Config::builtin());
    };
    if path.exists() {
        return load_valid_config(&path);
    }
    eprintln!(
        "sizer: no configuration file at {}; using built-in presets",
        path.display()
    );
    Ok(Config::builtin())
}

/// A Wayland session plus an X11 backend means only XWayland clients are visible.
fn warn_if_wayland_session() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        eprintln!(
            "sizer: warning: this is a Wayland session; the X11 backend only sees XWayland \
             windows (use the GNOME shell extension for native Wayland windows)"
        );
    }
}

fn report_apply(json: bool, id: &WindowId, result: &GeometryResult, area: Rect) -> CliResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "window": id,
                "result": result,
                "area": area,
            }))?
        );
        return Ok(());
    }
    let (dx, dy, dw, dh) = result.delta();
    println!(
        "requested {} {}x{} at {},{}",
        basis_label(result.basis),
        result.requested.w,
        result.requested.h,
        result.requested.x,
        result.requested.y
    );
    println!(
        "achieved  {} {}x{} at {},{}",
        basis_label(result.basis),
        result.achieved.w,
        result.achieved.h,
        result.achieved.x,
        result.achieved.y
    );
    println!(
        "frame     {}x{} at {},{}",
        result.achieved_frame.w,
        result.achieved_frame.h,
        result.achieved_frame.x,
        result.achieved_frame.y
    );
    println!(
        "client    {}x{} at {},{}",
        result.achieved_client.w,
        result.achieved_client.h,
        result.achieved_client.x,
        result.achieved_client.y
    );
    println!(
        "delta     {dx},{dy},{dw},{dh} ({})",
        if result.is_exact() {
            "exact"
        } else {
            "not exact"
        }
    );
    let mut flags = Vec::new();
    if result.clamped {
        flags.push("clamped");
    }
    if result.adjusted_by_hints {
        flags.push("adjusted_by_hints");
    }
    if result.ignored {
        flags.push("ignored");
    }
    if result.unchanged {
        flags.push("unchanged");
    }
    if result.unmaximized {
        flags.push("unmaximized");
    }
    println!(
        "flags     {}",
        if flags.is_empty() {
            "none".to_string()
        } else {
            flags.join(" ")
        }
    );
    println!("area      {}x{} at {},{}", area.w, area.h, area.x, area.y);
    Ok(())
}

fn resolve(backend: &X11Backend, text: &str) -> CliResult<WindowId> {
    Ok(resolve_selector(backend, &parse_selector(text)?)?)
}

fn parse_selector(text: &str) -> Result<WindowSelector, String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("active") {
        return Ok(WindowSelector::Active);
    }
    if let Some(rest) = trimmed.strip_prefix("title:") {
        return Ok(WindowSelector::TitleContains(rest.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("app:") {
        return Ok(WindowSelector::AppId(rest.to_string()));
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .filter(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return u32::from_str_radix(hex, 16)
            .map(|id| WindowSelector::Id(WindowId(id.to_string())))
            .map_err(|err| err.to_string());
    }
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Ok(WindowSelector::Id(WindowId(trimmed.to_string())));
    }
    Err(format!(
        "window selector {text:?} is not active, <id>, 0x<id>, title:<text> or app:<class>"
    ))
}

fn parse_basis(text: &str) -> Result<SizeBasis, String> {
    match text.to_ascii_lowercase().as_str() {
        "client" | "content" => Ok(SizeBasis::Client),
        "frame" | "window" => Ok(SizeBasis::Frame),
        other => Err(format!("unknown basis {other:?}, expected client or frame")),
    }
}

fn parse_area(text: &str) -> Result<AreaKind, String> {
    match text.to_ascii_lowercase().as_str() {
        "workarea" | "work" => Ok(AreaKind::WorkArea),
        "monitor" | "screen" => Ok(AreaKind::Monitor),
        other => Err(format!(
            "unknown area {other:?}, expected workarea or monitor"
        )),
    }
}

fn parse_monitor(text: &str) -> Result<MonitorSelector, String> {
    match text.to_ascii_lowercase().as_str() {
        "active" => Ok(MonitorSelector::Active),
        "primary" => Ok(MonitorSelector::Primary),
        other => other
            .parse::<usize>()
            .map(MonitorSelector::Index)
            .map_err(|_| {
                format!("unknown monitor {other:?}, expected active, primary or an index")
            }),
    }
}

fn parse_anchor(text: &str) -> Result<Option<Anchor>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    Anchor::parse(text)
        .map(Some)
        .ok_or_else(|| format!("unknown anchor {text:?}; use tl, t, tr, l, c, r, bl, b, br"))
}

fn parse_pixels(text: &str) -> Result<(i32, i32), String> {
    match SizeSpec::parse(text) {
        Some(SizeSpec::Pixels(width, height)) => Ok((width, height)),
        Some(SizeSpec::Percent(..)) => Err(format!(
            "size {text:?} is a percentage; use --percent for that"
        )),
        None => Err(format!(
            "size {text:?} must be 1..={} pixels, like 1024x768",
            SizeSpec::MAX_DIMENSION
        )),
    }
}

/// `50x50` or `50%x50%`: both halves are percentages of the reference area.
fn parse_percent(text: &str) -> Result<(f32, f32), String> {
    let (width, height) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("percent {text:?} must look like 50x50"))?;
    let parse = |value: &str| {
        let value = value.trim();
        let value = value.strip_suffix('%').unwrap_or(value).trim();
        value
            .parse::<f32>()
            .ok()
            .filter(|value| *value > 0.0 && *value <= SizeSpec::MAX_PERCENT)
    };
    match (parse(width), parse(height)) {
        (Some(width), Some(height)) => Ok((width, height)),
        _ => Err(format!(
            "percent {text:?} must be 0 < p <= {}, like 50x50",
            SizeSpec::MAX_PERCENT
        )),
    }
}

fn basis_label(basis: SizeBasis) -> &'static str {
    match basis {
        SizeBasis::Client => "client ",
        SizeBasis::Frame => "frame  ",
    }
}

fn geometry_write_label(behavior: sizer_platform::Behavior) -> &'static str {
    match behavior.geometry_write {
        sizer_platform::GeometryWrite::EwmhMoveresize => "_NET_MOVERESIZE_WINDOW",
        sizer_platform::GeometryWrite::FrameMoveResize => "move_resize_frame",
    }
}

fn describe_monitor(monitor: &Monitor) -> String {
    format!(
        "{} {}x{} at {},{} {}",
        monitor.name,
        monitor.rect.w,
        monitor.rect.h,
        monitor.rect.x,
        monitor.rect.y,
        if monitor.primary { "primary" } else { "" }
    )
    .trim_end()
    .to_string()
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pixel_sizes_and_percentages() {
        assert_eq!(parse_pixels("1024x768"), Ok((1024, 768)));
        assert!(parse_pixels("50%x50%").is_err());
        assert!(parse_pixels("wide").is_err());
        assert_eq!(parse_percent("50x50"), Ok((50.0, 50.0)));
        assert_eq!(parse_percent("33.5% x 50%"), Ok((33.5, 50.0)));
        assert!(parse_percent("0x50").is_err());
        assert!(parse_percent("wide").is_err());
        // A percentage that would resolve past i32 is rejected at the argument.
        assert!(parse_percent("1e30x50").is_err());
        assert!(parse_pixels("40000x600").is_err());
    }

    #[test]
    fn invalid_accelerators_become_problems() {
        assert!(hotkey_problem("options.hotkey".to_string(), "<Primary><Super>z").is_none());
        let problem = hotkey_problem("options.hotkey".to_string(), "<Hyper>q")
            .expect("an unsupported modifier must be reported");
        assert_eq!(problem.field, "options.hotkey");
    }

    fn preset(label: &str, group: Option<&str>) -> Preset {
        Preset {
            label: label.to_string(),
            size: "800x600".to_string(),
            group: group.map(str::to_string),
            anchor: None,
            hotkey: None,
            monitor: None,
            x: None,
            y: None,
            allow_outside_bounds: None,
            kind: sizer_config::PresetKind::Resize,
        }
    }

    /// A window with 4px borders and a 24px title bar: frame 808x628, client 800x600.
    fn decorated_geometry() -> Geometry {
        Geometry {
            frame: Rect::new(100, 100, 808, 628),
            client: Rect::new(104, 124, 800, 600),
            scale: 1.0,
            decorated: true,
        }
    }

    #[test]
    fn saved_size_uses_the_basis_apply_uses() {
        let geometry = decorated_geometry();
        assert_eq!(saved_size(&geometry, SizeBasis::Client), "800x600");
        assert_eq!(saved_size(&geometry, SizeBasis::Frame), "808x628");
    }

    #[test]
    fn saved_size_round_trips_through_the_preset_parser() {
        let geometry = decorated_geometry();
        let area = Rect::new(0, 0, 1920, 1080);
        for (basis, expected) in [
            (SizeBasis::Client, (800, 600)),
            (SizeBasis::Frame, (808, 628)),
        ] {
            let preset = build_preset(
                "round-trip",
                Some(saved_size(&geometry, basis).as_str()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("saved sizes are valid presets");
            // `sizer apply` resolves the preset size in the configured basis, so
            // re-applying it must reproduce the captured rectangle exactly.
            assert_eq!(preset.size_spec().unwrap().resolve(area), expected);
        }
    }

    #[test]
    fn key_events_are_matched_by_modifiers_too() {
        let super_alt = preset("super-alt", None);
        let ctrl_alt = preset("ctrl-alt", None);
        // F8 has the same keycode under every modifier combination.
        let keymap: Vec<(u8, u16, &Preset)> =
            vec![(74, 8 | 64, &super_alt), (74, 8 | 4, &ctrl_alt)];
        assert_eq!(
            preset_for_key(&keymap, 74, 8 | 64).map(|p| p.label.as_str()),
            Some("super-alt")
        );
        assert_eq!(
            preset_for_key(&keymap, 74, 8 | 4).map(|p| p.label.as_str()),
            Some("ctrl-alt")
        );
        assert!(preset_for_key(&keymap, 74, 8).is_none());
        assert!(preset_for_key(&keymap, 73, 8 | 64).is_none());
    }

    #[test]
    fn extension_failures_are_told_apart() {
        assert_eq!(
            classify_extension_failure("org.freedesktop.DBus.Error.ServiceUnknown", "x"),
            ExtensionFailure::NotRunning
        );
        assert_eq!(
            classify_extension_failure("org.freedesktop.DBus.Error.NameHasNoOwner", "x"),
            ExtensionFailure::NotRunning
        );
        assert_eq!(
            classify_extension_failure("org.freedesktop.DBus.Error.UnknownMethod", "x"),
            ExtensionFailure::TooOld
        );
        assert_eq!(
            classify_extension_failure("org.freedesktop.DBus.Error.UnknownInterface", "x"),
            ExtensionFailure::TooOld
        );
        assert_eq!(
            classify_extension_failure(
                "org.sizerlinux.Shell.Error",
                "org.sizerlinux.Shell.Error: no pointer"
            ),
            ExtensionFailure::CallFailed("org.sizerlinux.Shell.Error: no pointer".to_string())
        );
    }

    #[test]
    fn extension_failure_advice_matches_the_cause() {
        let not_running = ExtensionFailure::NotRunning.describe();
        assert!(not_running.contains("gnome-extensions enable sizer@sizerlinux.dev"));
        let too_old = ExtensionFailure::TooOld.describe();
        assert!(too_old.contains("older than this sizer"));
        // An installed-but-old extension must not be reported as missing.
        assert!(!too_old.contains("gnome-extensions enable"));
        let failed = ExtensionFailure::CallFailed("boom".to_string()).describe();
        assert!(failed.contains("boom"));
    }

    #[test]
    fn menu_failure_notification_names_the_cause() {
        let not_running = menu_failure_notification("no menu", Some(&ExtensionFailure::NotRunning));
        assert!(not_running.contains("no menu"));
        assert!(not_running.contains("gnome-extensions enable sizer@sizerlinux.dev"));
        let too_old = menu_failure_notification("no menu", Some(&ExtensionFailure::TooOld));
        assert!(too_old.contains("older than this sizer"));
        assert!(!too_old.contains("gnome-extensions enable"));
        let failed = menu_failure_notification(
            "no menu",
            Some(&ExtensionFailure::CallFailed(
                "org.sizerlinux.Shell.Error: boom".to_string(),
            )),
        );
        assert!(failed.contains("boom"));
        // No extension involvement at all: keep the hotkey hint.
        let plain = menu_failure_notification("cannot open display", None);
        assert!(plain.contains("cannot open display"));
        assert!(plain.contains("Ctrl+Win+Z"));
    }

    #[test]
    fn lock_bits_do_not_change_the_hotkey_identity() {
        let active = preset("active", None);
        let keymap: Vec<(u8, u16, &Preset)> = vec![(74, 8 | 64, &active)];
        let with_caps_and_num = sizer_backend_x11::hotkey_modifiers(8 | 64 | 2 | 16);
        assert_eq!(with_caps_and_num, 8 | 64);
        assert_eq!(
            preset_for_key(&keymap, 74, with_caps_and_num).map(|p| p.label.as_str()),
            Some("active")
        );
        // Button1Mask..Button5Mask arrive in the key event while a mouse button
        // is held and must not turn the hotkey into a miss.
        let with_mouse_button = sizer_backend_x11::hotkey_modifiers(8 | 64 | 0x0100 | 0x1000);
        assert_eq!(with_mouse_button, 8 | 64);
        assert_eq!(
            preset_for_key(&keymap, 74, with_mouse_button).map(|p| p.label.as_str()),
            Some("active")
        );
    }

    #[test]
    fn preset_order_is_scoped_to_its_group() {
        let mut config = Config {
            version: 1,
            options: Default::default(),
            presets: vec![
                preset("a", Some("G")),
                preset("x", None),
                preset("b", Some("G")),
                preset("c", Some("G")),
            ],
        };
        // Move "c" to the top of G; the group keeps its menu slot and the
        // ungrouped preset keeps its own order.
        reorder_preset(&mut config, 3, 1).unwrap();
        let labels: Vec<&str> = config.presets.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["c", "a", "x", "b"]);
        assert!(reorder_preset(&mut config, 0, 9).is_err());
        assert!(reorder_preset(&mut config, 0, 0).is_err());
    }

    #[test]
    fn duplicate_labels_are_rejected_for_the_same_normalized_group() {
        let config = Config {
            version: 1,
            options: Default::default(),
            presets: vec![preset("1080p", Some("16:9"))],
        };
        let mut candidate = preset("1080P", Some(" 16:9 / "));
        assert!(ensure_unique(&config, &candidate, None).is_err());
        // A different group may reuse the label.
        candidate.group = Some("YouTube".to_string());
        assert!(ensure_unique(&config, &candidate, None).is_ok());
    }

    #[test]
    fn invalid_preset_fields_are_rejected() {
        let mut candidate = preset("ok", None);
        assert!(check_preset(&candidate).is_ok());
        candidate.size = "wide".to_string();
        assert!(check_preset(&candidate).is_err());
        candidate.size = "800x600".to_string();
        candidate.label = "  ".to_string();
        assert!(check_preset(&candidate).is_err());
        candidate.label = "ok".to_string();
        candidate.hotkey = Some("<Hyper>q".to_string());
        assert!(check_preset(&candidate).is_err());
        candidate.hotkey = None;
        candidate.monitor = Some("left".to_string());
        assert!(check_preset(&candidate).is_err());
    }

    #[test]
    fn position_is_a_pair_and_replaces_the_anchor() {
        assert!(
            build_preset("p", Some("100x100"), None, None, None, None, Some(10), None).is_err()
        );
        assert!(
            build_preset(
                "p",
                Some("100x100"),
                None,
                Some("center"),
                None,
                None,
                Some(10),
                Some(20)
            )
            .is_err()
        );
        let preset = build_preset(
            "p",
            Some("100x100"),
            None,
            None,
            None,
            Some("primary"),
            Some(-10),
            Some(20),
        )
        .expect("a position with a monitor is valid");
        assert_eq!(preset.position(), Some((-10, 20)));
        assert_eq!(preset.monitor.as_deref(), Some("primary"));
        assert!(preset.anchor.is_none());
    }

    #[test]
    fn a_move_only_preset_needs_a_position() {
        let mut moves = preset("relocate", None);
        moves.size = String::new();
        moves.x = Some(10);
        moves.y = Some(20);
        assert!(check_preset(&moves).is_ok());
        moves.x = None;
        assert!(check_preset(&moves).is_err());
    }

    #[test]
    fn unicode_labels_and_groups_round_trip() {
        let candidate = build_preset(
            "Chinese Preset 500x300",
            Some("500x300"),
            Some(" Resolution / Chinese "),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(candidate.label, "Chinese Preset 500x300");
        assert_eq!(candidate.group.as_deref(), Some("Resolution / Chinese"));
    }
}
