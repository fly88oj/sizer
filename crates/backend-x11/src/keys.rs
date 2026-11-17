//! Accelerator parsing, passive grabs and the delivery probe.
//!
//! Phase 0 result: `XGrabKey` succeeding is not evidence that the hotkey works —
//! with a window manager running, the injected combination never reached the
//! grabbing client. Registration therefore has to attempt delivery and report
//! what actually arrived.

use std::thread::sleep;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

use sizer_platform::{BackendError, BackendResult};

pub const MOD_SHIFT: u16 = 1;
pub const MOD_CONTROL: u16 = 4;
pub const MOD_MOD1: u16 = 8;
pub const MOD_MOD2: u16 = 16;
pub const MOD_LOCK: u16 = 2;
pub const MOD_MOD4: u16 = 64;
/// Button1Mask..Button5Mask (0x0100..0x1000, X11 `X.h`). Pointer buttons are
/// part of the key event state and must not be mistaken for a hotkey modifier.
pub const MOD_BUTTONS: u16 = 0x1f00;

const KEYSYM_SHIFT_L: u32 = 0xffe1;
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_ALT_L: u32 = 0xffe9;
const KEYSYM_SUPER_L: u32 = 0xffeb;

/// A parsed accelerator such as `<Super><Alt>s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accelerator {
    pub mask: u16,
    pub keysym: u32,
}

/// Why a grab did not end with a usable registration.
#[derive(Debug)]
pub enum GrabFailure {
    /// Another client owns the combination (`BadAccess`).
    Conflict(String),
    /// Anything else, including a lost connection.
    Other(String),
}

/// Parse `<Super><Alt>s`, `Ctrl+Alt+1` or `super+alt+F10`.
pub fn parse_accelerator(text: &str) -> BackendResult<Accelerator> {
    let mut mask = 0u16;
    let mut keysym = None;
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
    if parts.is_empty() {
        return Err(BackendError::Protocol(format!(
            "empty accelerator {text:?}"
        )));
    }
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "shift" => mask |= MOD_SHIFT,
            "ctrl" | "control" | "primary" => mask |= MOD_CONTROL,
            "alt" | "mod1" => mask |= MOD_MOD1,
            "super" | "win" | "meta" | "mod4" => mask |= MOD_MOD4,
            other => {
                if keysym.is_some() {
                    return Err(BackendError::Protocol(format!(
                        "accelerator {text:?} names more than one key"
                    )));
                }
                keysym = Some(keysym_for(other).ok_or_else(|| {
                    BackendError::Protocol(format!("unsupported key {other:?} in accelerator"))
                })?);
            }
        }
    }
    let keysym =
        keysym.ok_or_else(|| BackendError::Protocol(format!("accelerator {text:?} has no key")))?;
    Ok(Accelerator { mask, keysym })
}

fn keysym_for(name: &str) -> Option<u32> {
    if let Some(rest) = name.to_ascii_uppercase().strip_prefix('F') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=35).contains(&n) {
                return Some(0xffbe + (n - 1));
            }
        }
    }
    if name.len() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_alphanumeric() {
            return Some(ch.to_ascii_lowercase() as u32);
        }
        let sym = match ch {
            ' ' => 0x0020,
            '!' => 0x0021,
            '"' => 0x0022,
            '#' => 0x0023,
            '$' => 0x0024,
            '%' => 0x0025,
            '&' => 0x0026,
            '\'' => 0x0027,
            '(' => 0x0028,
            ')' => 0x0029,
            '*' => 0x002a,
            '+' => 0x002b,
            ',' => 0x002c,
            '-' => 0x002d,
            '.' => 0x002e,
            '/' => 0x002f,
            ':' => 0x003a,
            ';' => 0x003b,
            '<' => 0x003c,
            '=' => 0x003d,
            '>' => 0x003e,
            '?' => 0x003f,
            '@' => 0x0040,
            '[' => 0x005b,
            '\\' => 0x005c,
            ']' => 0x005d,
            '^' => 0x005e,
            '_' => 0x005f,
            '`' => 0x0060,
            '{' => 0x007b,
            '|' => 0x007c,
            '}' => 0x007d,
            '~' => 0x007e,
            _ => return None,
        };
        return Some(sym);
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "space" => 0x0020,
        "tab" => 0xff09,
        "return" | "enter" => 0xff0d,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" => 0xff55,
        "pagedown" | "page_down" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "print" => 0xff61,
        "pause" => 0xff13,
        _ => return None,
    })
}

/// First keycode bound to `keysym`, ignoring modifier-only mappings.
pub fn keycode_for(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let mapping = conn
        .get_keyboard_mapping(min, max - min + 1)
        .ok()?
        .reply()
        .ok()?;
    let per = mapping.keysyms_per_keycode as usize;
    for (index, chunk) in mapping.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            return Some(min + index as u8);
        }
    }
    None
}

/// Grab the combination with the num-lock / caps-lock variants a real desktop
/// produces, otherwise the hotkey dies whenever caps lock is on.
pub fn grab(
    conn: &RustConnection,
    root: Window,
    mask: u16,
    keycode: u8,
) -> Result<(), GrabFailure> {
    for extra in [0u16, MOD_MOD2, MOD_LOCK, MOD_LOCK | MOD_MOD2] {
        let result = conn
            .grab_key(
                false,
                root,
                ModMask::from(mask | extra),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|err| GrabFailure::Other(err.to_string()))?
            .check();
        if let Err(err) = result {
            return Err(match err {
                ReplyError::X11Error(x11) if x11.error_kind == ErrorKind::Access => {
                    GrabFailure::Conflict(format!(
                        "X server refused the grab with BadAccess: another client owns this combination ({x11:?})"
                    ))
                }
                other => GrabFailure::Other(other.to_string()),
            });
        }
    }
    Ok(())
}

pub fn ungrab(conn: &RustConnection, root: Window, keycode: u8) -> BackendResult<()> {
    conn.ungrab_key(keycode, root, ModMask::ANY)
        .map_err(|err| BackendError::Protocol(err.to_string()))?
        .check()
        .map_err(|err| BackendError::Protocol(err.to_string()))
}

/// Press the combination through XTEST, then wait for our own passive grab to
/// deliver it back. Only call this on a display where injecting input is safe.
pub fn inject_and_wait(
    conn: &RustConnection,
    root: Window,
    acc: Accelerator,
    keycode: u8,
    timeout: Duration,
) -> BackendResult<bool> {
    conn.xtest_get_version(2, 2)
        .map_err(|err| BackendError::Protocol(err.to_string()))?
        .reply()
        .map_err(|err| BackendError::Protocol(format!("XTEST unavailable: {err}")))?;

    let mut modifiers = Vec::new();
    for (bit, keysym) in [
        (MOD_SHIFT, KEYSYM_SHIFT_L),
        (MOD_CONTROL, KEYSYM_CONTROL_L),
        (MOD_MOD1, KEYSYM_ALT_L),
        (MOD_MOD4, KEYSYM_SUPER_L),
    ] {
        if acc.mask & bit != 0 {
            if let Some(code) = keycode_for(conn, keysym) {
                modifiers.push(code);
            }
        }
    }

    let fake = |type_: u8, code: u8| -> BackendResult<()> {
        conn.xtest_fake_input(type_, code, 0, root, 0, 0, 0)
            .map_err(|err| BackendError::Protocol(err.to_string()))?
            .check()
            .map_err(|err| BackendError::Protocol(err.to_string()))
    };
    for code in &modifiers {
        fake(2, *code)?;
    }
    fake(2, keycode)?;
    fake(3, keycode)?;
    for code in modifiers.iter().rev() {
        fake(3, *code)?;
    }
    conn.flush()
        .map_err(|err| BackendError::Protocol(err.to_string()))?;

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(Event::KeyPress(press))) if press.detail == keycode => return Ok(true),
            Ok(Some(_)) => {}
            Ok(None) => sleep(Duration::from_millis(40)),
            Err(err) => return Err(BackendError::Protocol(err.to_string())),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_angle_form_with_several_modifiers() {
        let parsed = parse_accelerator("<Super><Alt>F8").expect("angle form");
        assert_eq!(parsed.mask, MOD_MOD4 | MOD_MOD1);
        assert_eq!(parsed.keysym, 0xffbe + 7);

        let parsed = parse_accelerator("<Super><Alt><Ctrl>s").expect("three modifiers");
        assert_eq!(parsed.mask, MOD_MOD4 | MOD_MOD1 | MOD_CONTROL);
        assert_eq!(parsed.keysym, 's' as u32);

        let parsed = parse_accelerator("Ctrl+Alt+1").expect("plus form");
        assert_eq!(parsed.mask, MOD_CONTROL | MOD_MOD1);
        assert_eq!(parsed.keysym, '1' as u32);

        assert!(parse_accelerator("<Super><Alt>").is_err());
        assert!(parse_accelerator("<Super>zk").is_err());
    }
}
