//! Native X11 drag-and-drop using XDND protocol
//!
//! This module provides direct access to X11's XDND (X Drag-and-Drop) protocol,
//! allowing us to track which window the pointer is over during drag operations.
//!
//! Currently only provides window registration with XdndAware property.
//! Event callback functionality was removed as unused.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt, Window, Atom};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

/// XDND protocol version we support
const XDND_VERSION: u32 = 5;

/// XDND atoms needed for window registration
struct XdndAtoms {
    xdnd_aware: Atom,
}

/// Global state for X11 drag-and-drop
struct X11DndState {
    /// X11 connection
    conn: Option<RustConnection>,
    /// XDND atoms
    atoms: Option<XdndAtoms>,
    /// Map from X11 window ID to window label
    window_to_label: HashMap<Window, String>,
}

/// Global X11 DnD state
fn global_state() -> &'static Mutex<Option<X11DndState>> {
    static INSTANCE: OnceLock<Mutex<Option<X11DndState>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Intern an atom, returning 0 on error
fn intern_atom(conn: &RustConnection, name: &str) -> Atom {
    match conn.intern_atom(false, name.as_bytes()) {
        Ok(cookie) => {
            match cookie.reply() {
                Ok(reply) => reply.atom,
                Err(_) => 0,
            }
        }
        Err(_) => 0,
    }
}

/// Initialize the X11 DnD system
/// Returns Ok(true) if X11 is available, Ok(false) if not, Err on error
pub fn init() -> Result<bool, String> {
    // Check if we're on X11
    if std::env::var("DISPLAY").is_err() {
        eprintln!("[TiddlyDesktop] X11: DISPLAY not set, skipping X11 DnD init");
        return Ok(false);
    }

    // Don't init if we're on Wayland (XWayland might set DISPLAY but we prefer native Wayland)
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        eprintln!("[TiddlyDesktop] X11: Running on Wayland, skipping X11 DnD init");
        return Ok(false);
    }

    eprintln!("[TiddlyDesktop] X11: Initializing native XDND protocol handler");

    // Connect to X11
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| format!("Failed to connect to X11: {}", e))?;

    let _screen = &conn.setup().roots[screen_num];

    // Intern XDND atoms (only the ones we need)
    let atoms = XdndAtoms {
        xdnd_aware: intern_atom(&conn, "XdndAware"),
    };

    eprintln!(
        "[TiddlyDesktop] X11: Interned atoms - XdndAware={}",
        atoms.xdnd_aware
    );

    // Store in global state
    if let Ok(mut guard) = global_state().lock() {
        *guard = Some(X11DndState {
            conn: Some(conn),
            atoms: Some(atoms),
            window_to_label: HashMap::new(),
        });
    }

    eprintln!("[TiddlyDesktop] X11: Native XDND protocol handler initialized");

    Ok(true)
}

/// Register a window with its label
pub fn register_window(window: Window, label: &str) {
    if let Ok(mut guard) = global_state().lock() {
        if let Some(ref mut state) = *guard {
            state.window_to_label.insert(window, label.to_string());
            eprintln!(
                "[TiddlyDesktop] X11: Registered window {} for '{}'",
                window, label
            );

            // Set XdndAware property on the window
            if let (Some(ref conn), Some(ref atoms)) = (&state.conn, &state.atoms) {
                let version = XDND_VERSION;
                let _ = conn.change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    atoms.xdnd_aware,
                    xproto::AtomEnum::ATOM,
                    &[version],
                );
                let _ = conn.flush();
                eprintln!(
                    "[TiddlyDesktop] X11: Set XdndAware on window {} (version {})",
                    window, version
                );
            }
        }
    }
}
