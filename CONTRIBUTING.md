# Contributing

Thanks for considering a contribution to Sizer. This document covers the setup, the test layers, the release gate, and the conventions that keep things smooth.

## Prerequisites

| Tool | Purpose | Minimum version |
|------|---------|-----------------|
| Rust | Compiler | 1.85 (edition 2024) |
| GTK 4 | Config editor | 4.6 |
| glib-compile-schemas | GNOME extension schema | any |
| Xvfb | Headless X server for e2e | any |
| gnome-shell | Nested Wayland for e2e | 46+ |
| python-xlib | X11 e2e harness | any |
| PyGObject | GNOME e2e harness | any |
| gettext | Compiling MO catalogs | any |

```bash
# Debian / Ubuntu one-liner
sudo apt install build-essential pkg-config libgtk-4-dev libglib2.0-dev \
                 libpango1.0-dev libx11-dev libxrandr-dev libxtst-dev \
                 libxi-dev gettext python3-xlib python3-gi xvfb \
                 xrandr xprop gnome-shell
```

## Build

```bash
cargo build --workspace          # debug
cargo build --release -p sizer-cli   # release binary only
```

## Test Layers

The project has four test layers. Run them from the repo root:

| Layer | Command | Coverage |
|-------|---------|----------|
| Rust unit | `cargo test --workspace` | geometry model, config parser, macro lexer/parser/eval |
| Extension JS unit | `gjs -m tests/unit/preset_state_test.js` | preset bookkeeping, no shell imports |
| Harness unit | `python3 tests/unit/wait_applied_test.py` | applied-result matching |
| X11 e2e | `make e2e-x11` | gnome-shell acting as WM under Xvfb |
| GNOME e2e | `make e2e-gnome` | shell extension in a nested compositor |
| deb e2e | `make e2e-deb` | `apt install` + run + `apt purge` in a container |
| Release gate | **`make gate`** | All of the above; fails fast |

`make gate` is what matters — if any layer fails, no release happens. Individual layer failure modes are different:

- Rust unit: `cargo test` returns non-zero on first failure.
- X11 e2e drives the release build against a real WM; debug-only results are not trusted.
- GNOME e2e boots a throwaway session bus and compositor with the extension installed.
- deb e2e spins up a clean container, installs the deb, runs the binary, and purges to verify no leftovers.

## Lint and Format

```bash
cargo fmt          # format
cargo fmt --check  # CI gate (must be clean)
cargo clippy --workspace --all-targets -- -D warnings  # CI gate
```

## Commit Conventions

- Single-language bodies (no bilingual)
- One logical change per commit; split early rather than late
- Author identity is the repo's configured git config — do not hard-code names or emails
- `git add -A` is forbidden; stage files one by one
- No `amend`; it can race with other writers on the same branch

## Pull Request Checklist

- [ ] Tests pass at all four layers (`make gate` or at least unit + X11 e2e locally)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -- -D warnings` clean
- [ ] No Chinese characters in source strings (except intentional test fixtures)
- [ ] New user-facing strings wrapped with `gettext` catalogs in mind; PO updated in a follow-up PR if necessary
- [ ] `packaging/_common.sh` updated when adding files that must ship in every format

## Adding or Modifying a Preset Field

1. Add to `crates/config/src/lib.rs` → the `Config` / `Preset` / `MenuNode` types derive `Serialize` / `Deserialize`.
2. Update `sizer_xml.rs` if the field is also part of the Sizer XML import format.
3. Update `configure.rs` GTK forms so the config editor reflects the change.
4. Update `locale/sizer.pot` if new user-facing labels or messages were introduced.
5. Run `make gate` — the deb e2e and GNOME harness are particularly good at catching silent regressions.

## GNOME Extension Development

The extension lives at `platform/gnome-extension/sizer@sizerlinux.dev/`. Load it in a nested shell without installing:

```bash
export GNOME_SHELL_EXTENSIONS_DIR=$PWD/platform
gnome-shell --nested --wayland --display-server &
# then enable in the shell with Looking Glass or gnome-extensions enable
```

DO NOT test inside the real `:0` desktop session — it will race with the user's window manager and can corrupt their session state.

## A Note About Window Writing

All geometry write paths ultimately call `_NET_MOVERESIZE_WINDOW` on X11 and `meta_window_move_resize_frame()` on GNOME. This is intentional — we do not call the raw X11 `ConfigureWindow` protocol because Mutter ignores it for client-decorated windows, and we do not fake-move windows by changing their decorations. If you see a "write succeeded but geometry didn't change" bug, it is almost certainly a compositor limitation, not an API error.
