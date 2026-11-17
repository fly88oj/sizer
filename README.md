# Sizer

Precise window sizing and preset control for Linux — a drop-in replacement for the original Sizer utility, rewritten in Rust.

- **GNOME Shell (Wayland)** — extension + D-Bus daemon, fully featured
- **X11** — cursor popup menu, tray icon, keyboard-driven resize
- **Scriptable** — every GUI action has a CLI sibling; JSON output throughout
- **Macro language** — size, position, minimize, maximize multiple windows in one run
- **i18n ready** — English source, PO catalogs for 7 languages

```bash
# The menu at the pointer — this is what the desktop entry runs
sizer menu

# Apply a preset to the active window
sizer apply "16:9 / 1280x720"

# Open the config editor (GTK)
sizer config edit

# Run a macro against all windows
sizer macro "window 1 resize 1280 720 anchor top-left"

# Diagnose your session
sizer doctor
```

## Architecture

```
┌─────────────────────────────────────────────────┐
│  CLI (clap)  ─── tray ─── daemon ─── config    │
└──────────────┬──────────────────────────────────┘
               │  geometry model (core)
               │  preset model  (config)
               │  macro engine  (macro)
      ┌────────┴────────┐
      ▼                 ▼
┌──────────┐    ┌──────────────┐
│  X11     │    │  GNOME Wayland│
│ backend  │    │  extension   │
└──────────┘    └──────────────┘
```

The core model is pure Rust. Two backends sit behind a platform trait:

| Backend | Tier | Delivery path |
|---------|------|---------------|
| GNOME Shell 46–50 | 1 (primary) | Shell extension + D-Bus + user daemon |
| X11 (xcb) | 2 | `_NET_MOVERESIZE_WINDOW`, Xft cursor menu |
| KWin | 3 | JS script (not yet implemented) |
| Sway / Hyprland | 3 | IPC client (not yet implemented) |

## Installation

### From a release artifact

| Distro | Command |
|--------|---------|
| Debian / Ubuntu | `sudo apt install ./sizer_0.1.0_amd64.deb` |
| Fedora / RHEL | `sudo dnf install sizer-0.1.0-1.x86_64.rpm` |
| Arch | `pacman -U sizer-0.1.0-1-x86_64.pkg.tar.zst` |
| AppImage | `chmod +x sizer-0.1.0-x86_64.AppImage && ./sizer-0.1.0-x86_64.AppImage` |
| Flatpak | `flatpak install flathub dev.sizer.sizer` (coming soon) |

The deb package also installs the GNOME Shell extension under `~/.local/share/gnome-shell/extensions/sizer@sizerlinux.dev` and a `.desktop` entry so `sizer menu` appears in your application launcher.

### Build from source

```bash
# Prerequisites
sudo apt install build-essential pkg-config libgtk-4-dev libglib2.0-dev \
                 libpango1.0-dev libx11-dev libxrandr-dev libxtst-dev \
                 libxi-dev gettext

# Build
cargo build --release -p sizer-cli

# Run directly
./target/release/sizer doctor
```

Supported Rust: 1.85+.

## Configuration

Sizer looks for `~/.config/sizer/config.toml`. Edit it by hand, or run `sizer config edit` for the GTK editor.

```toml
[[presets]]
label = "Half Screen"
size = "50%x50%"
anchor = "center"
group = "Quick"

[[presets]]
label = "1280x720"
size = "1280x720"
hotkey = "<Super>7"

[[presets]]
type = "separator"
group = "Quick"
```

### Preset fields

| Field | Values | Notes |
|-------|--------|-------|
| `label` | free text | Shown in menus |
| `size` | `WxH` pixels or `W%xH%` percentages | Blank = keep current |
| `anchor` | `no_anchor`, `top_left` … `bottom_right`, `center` | |
| `group` | free text | Groups form submenus |
| `hotkey` | `"<Super><Alt>F8"` style | Optional, multi-modifier safe |
| `basis` | `client`, `frame` | Which rect `size` refers to |
| `monitor` | `active`, `primary`, `N` | |
| `area` | `workarea`, `monitor` | Workarea excludes docks |

### Monitor semantics

`monitor = "active"` means the monitor containing the pointer. On multi-monitor setups, when the pointer rests over an overlap, the smallest-area monitor wins — this matches what most users expect "the monitor I'm on" to mean.

## The Macro Language

Macros describe a sequence of window operations. Window references are 1-indexed by creation order (`window 1` is the oldest), matching `_NET_CLIENT_LIST` on X11 and `stable_sequence` in GNOME.

```
window 1 resize 1280 720 anchor top-left
window 2 maximize
window 3 minimize
monitor 2 window 1 resize 800 600
```

Run with `sizer macro [TEXT]` or pipe stdin. Exit code is non-zero when any requested geometry could not be achieved. The full railroad grammar lives in `crates/macro/`.

## CLI Reference

```
sizer                     # menu at the pointer (desktop entry)
sizer apply <LABEL>       # apply a configured preset
sizer resize <WINDOW> W H # raw geometry
sizer macro [TEXT]        # run a Sizer macro
sizer daemon              # push presets to the GNOME extension
sizer tray                # StatusNotifierItem tray
sizer hotkeys             # register all configured hotkeys
sizer config edit         # GTK config editor
sizer doctor              # session / backend diagnostics
sizer list                # controllable windows
sizer geometry <WINDOW>   # frame + client rect
```

Every command accepts `--json` for machine-readable output and `--config <FILE>` to point at an alternate config.

## GNOME Shell Extension

The extension UUID is `sizer@sizerlinux.dev`. It exposes one D-Bus interface on the session bus for the daemon and implements the preset menu and window resizing through Mutter's public API.

| Shell version | Status |
|---------------|--------|
| 46 (Ubuntu 24.04) | Verified |
| 50 (Ubuntu 26.04) | Verified |

## i18n

Source strings are English and wrapped with `gettext` catalogs. PO catalogs ship for:

- `zh_CN` — Simplified Chinese
- `ja` — Japanese
- `ko` — Korean
- `es` — Spanish
- `fr` — French
- `de` — German
- `pt_BR` — Brazilian Portuguese

Build MO catalogs with `msgfmt` and install them under `<prefix>/share/locale/<lang>/LC_MESSAGES/sizer.mo`. The daemon will silently use source strings when no catalog is found — English always works.

Contributing translations is easy: edit `locale/<lang>/LC_MESSAGES/sizer.po` and open a PR.

## Building All Packages

```bash
make pkg-all                     # deb + rpm + appimage + pacman + flatpak
make pkg-deb                     # one at a time
PACKAGING_SKIP_BUILD=1 make pkg-all  # skip cargo release rebuild
```

Every script stages through `packaging/_common.sh`, so all five formats ship the same binary, extension, desktop entries, and icon.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the setup, test layers, and commit conventions.

## License

MIT. See [LICENSE](LICENSE).
