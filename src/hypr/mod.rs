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
pub mod minimize;
pub mod model;
pub mod request;

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Locate Hyprland's IPC directory: `$XDG_RUNTIME_DIR/hypr/<sig>/`.
///
/// Hyprland before 0.40 used `/tmp/hypr/<sig>/`. That is not tried: Omarchy
/// needs a far newer Hyprland, and `/tmp` is shared, so a socket found there
/// could belong to another user posing as the compositor.
pub fn ipc_dir() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| anyhow!("HYPRLAND_INSTANCE_SIGNATURE unset — not running under Hyprland"))?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR unset — cannot find Hyprland's socket"))?;
    let p = PathBuf::from(runtime).join("hypr").join(&sig);
    if p.exists() {
        return Ok(p);
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

    /// The address as a number, which is how window capture names a window.
    /// Zero for an address that is not hex, which no window has.
    pub fn as_u64(&self) -> u64 {
        u64::from_str_radix(&self.0, 16).unwrap_or(0)
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
