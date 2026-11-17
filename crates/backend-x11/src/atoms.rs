//! EWMH / ICCCM atoms used by the X11 backend, interned once per connection.
//!
//! Phase 0 established that every one of these has to be read from the server at
//! runtime: `_NET_FRAME_EXTENTS` is a measured value (37px on the test WM, not a
//! constant) and `_NET_WORKAREA` changes with panels and desktops.

use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use sizer_platform::{BackendError, BackendResult};

macro_rules! atom_fields {
    ($($field:ident => $name:expr),* $(,)?) => {
        #[allow(missing_docs)]
        pub struct Atoms {
            $(pub $field: u32,)*
        }

        impl Atoms {
            pub fn intern(conn: &RustConnection) -> BackendResult<Self> {
                Ok(Self {
                    $($field: intern(conn, $name)?,)*
                })
            }
        }
    };
}

atom_fields! {
    client_list => "_NET_CLIENT_LIST",
    client_list_stacking => "_NET_CLIENT_LIST_STACKING",
    active_window => "_NET_ACTIVE_WINDOW",
    frame_extents => "_NET_FRAME_EXTENTS",
    wm_state => "_NET_WM_STATE",
    state_maximized_vert => "_NET_WM_STATE_MAXIMIZED_VERT",
    state_maximized_horz => "_NET_WM_STATE_MAXIMIZED_HORZ",
    state_above => "_NET_WM_STATE_ABOVE",
    state_hidden => "_NET_WM_STATE_HIDDEN",
    workarea => "_NET_WORKAREA",
    current_desktop => "_NET_CURRENT_DESKTOP",
    wm_desktop => "_NET_WM_DESKTOP",
    wm_change_state => "WM_CHANGE_STATE",
    wm_name => "_NET_WM_NAME",
    wm_window_type => "_NET_WM_WINDOW_TYPE",
    wm_pid => "_NET_WM_PID",
    normal_hints => "WM_NORMAL_HINTS",
    moveresize_window => "_NET_MOVERESIZE_WINDOW",
}

/// Window types that are not user-controllable application windows.
pub const NON_CONTROLLABLE_TYPES: &[&str] = &[
    "_NET_WM_WINDOW_TYPE_DESKTOP",
    "_NET_WM_WINDOW_TYPE_DOCK",
    "_NET_WM_WINDOW_TYPE_TOOLBAR",
    "_NET_WM_WINDOW_TYPE_MENU",
    "_NET_WM_WINDOW_TYPE_UTILITY",
    "_NET_WM_WINDOW_TYPE_SPLASH",
    "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
    "_NET_WM_WINDOW_TYPE_POPUP_MENU",
    "_NET_WM_WINDOW_TYPE_TOOLTIP",
    "_NET_WM_WINDOW_TYPE_NOTIFICATION",
    "_NET_WM_WINDOW_TYPE_COMBO",
    "_NET_WM_WINDOW_TYPE_DND",
];

fn intern(conn: &RustConnection, name: &str) -> BackendResult<u32> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|err| BackendError::Protocol(err.to_string()))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|err| BackendError::Protocol(err.to_string()))
}

/// Intern an atom that is only needed for window-type filtering.
pub fn intern_type(conn: &RustConnection, name: &str) -> BackendResult<u32> {
    intern(conn, name)
}
