//! Hyprland IPC.
//!
//! Two Unix sockets live in `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`:
//!
//! * `.socket.sock`  — request/response. Write a command, read the reply,
//!   connection closes. `j/`-prefixed commands return JSON.
//! * `.socket2.sock` — a push event stream, one `name>>data` line per event.
//!
//! Note the asymmetry introduced in Hyprland 0.56: the **event** stream still
//! uses the classic `name>>payload` text format, but **dispatch** now goes
//! through Lua (`hl.dsp.window.close()`). The old `/dispatch focuswindow
//! address:0x…` form is gone. See `dispatch.rs`.

pub mod dispatch;
pub mod events;
pub mod model;
pub mod request;

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Locate Hyprland's IPC directory.
///
/// Modern Hyprland uses `$XDG_RUNTIME_DIR/hypr/<sig>/`; the pre-0.40 location
/// was `/tmp/hypr/<sig>/`. The legacy path is still tried so the dock works on
/// older setups, but on this machine `/tmp/hypr` does not exist at all.
pub fn ipc_dir() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| anyhow!("HYPRLAND_INSTANCE_SIGNATURE unset — not running under Hyprland"))?;

    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(runtime).join("hypr").join(&sig);
        if p.exists() {
            return Ok(p);
        }
    }

    let legacy = PathBuf::from("/tmp/hypr").join(&sig);
    if legacy.exists() {
        return Ok(legacy);
    }

    Err(anyhow!("no Hyprland IPC directory for instance {sig}"))
}

pub fn request_socket() -> Result<PathBuf> {
    Ok(ipc_dir()?.join(".socket.sock"))
}

pub fn event_socket() -> Result<PathBuf> {
    Ok(ipc_dir()?.join(".socket2.sock"))
}

/// A window address.
///
/// Hyprland is inconsistent here: JSON carries `"0x5654cf797160"` while the
/// event stream emits the same address as bare `5654cf797160`. Both normalise
/// to the same value so the two sources can be cross-referenced.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub String);

impl Address {
    pub fn parse(s: &str) -> Self {
        let t = s.trim();
        Address(t.strip_prefix("0x").unwrap_or(t).to_ascii_lowercase())
    }

    /// The `0x`-prefixed form, as dispatchers and JSON expect.
    #[allow(dead_code)]
    pub fn prefixed(&self) -> String {
        format!("0x{}", self.0)
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{}", self.0)
    }
}
