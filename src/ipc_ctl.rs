//! The dock's own control socket.
//!
//! A layer-shell surface cannot grab `Super+1`: global shortcuts belong to the
//! compositor. So Hyprland binds the key to `omarchy-dockctl activate 1`,
//! which writes one line here, and the dock acts on it.
//!
//! The socket lives at `$XDG_RUNTIME_DIR/omarchy-dock.sock`, which is
//! user-private (mode 0700 on the directory itself), so no authentication is
//! needed beyond the filesystem.

use crate::event::{AppEvent, Sender};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// A request from `omarchy-dockctl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Focus, cycle, or launch the nth dock item (1-based, as typed by users).
    Activate(usize),
    /// Force the dock visible, hidden, or back to its configured behaviour.
    Reveal,
    Hide,
    ToggleAutohide,
    /// Re-read config and rebuild.
    Reload,
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("omarchy-dock.sock")
}

/// Parse one command line. Unknown verbs are rejected rather than ignored, so
/// a typo in a keybinding is visible instead of silently doing nothing.
pub fn parse(line: &str) -> Option<Control> {
    let mut parts = line.split_whitespace();
    match parts.next()? {
        "activate" => {
            let n: usize = parts.next()?.parse().ok()?;
            // 1-based on the wire because keybindings read `activate 1`.
            // `then` and not `then_some`: the latter evaluates `n - 1`
            // eagerly, which underflows and panics on `activate 0`.
            (n >= 1).then(|| Control::Activate(n - 1))
        }
        "reveal" => Some(Control::Reveal),
        "hide" => Some(Control::Hide),
        "toggle-autohide" => Some(Control::ToggleAutohide),
        "reload" => Some(Control::Reload),
        _ => None,
    }
}

/// Serve the control socket until the process exits.
pub async fn serve(tx: Sender) -> Result<()> {
    let path = socket_path();

    // A stale socket from a crashed instance would make bind() fail. Removing
    // it is safe: a live instance would still be holding the path, and we
    // check for that first by trying to connect.
    if path.exists() {
        if UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("another omarchy-dock is already running at {}", path.display());
        }
        std::fs::remove_file(&path).ok();
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    tracing::info!(path = %path.display(), "control socket listening");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "control socket accept failed");
                continue;
            }
        };

        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match parse(&line) {
                Some(cmd) => {
                    tracing::debug!(?cmd, "control command");
                    if tx.send(AppEvent::Control(cmd)).await.is_err() {
                        return Ok(());
                    }
                }
                None => tracing::warn!(line = %line, "unknown control command"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_is_one_based_on_the_wire() {
        // Keybindings read `activate 1` for the first icon.
        assert_eq!(parse("activate 1"), Some(Control::Activate(0)));
        assert_eq!(parse("activate 9"), Some(Control::Activate(8)));
        // Zero would silently mean "last item" if we just subtracted.
        assert_eq!(parse("activate 0"), None);
    }

    #[test]
    fn rejects_unknown_and_malformed() {
        assert_eq!(parse("frobnicate"), None);
        assert_eq!(parse("activate"), None);
        assert_eq!(parse("activate x"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn accepts_the_simple_verbs() {
        assert_eq!(parse("reveal"), Some(Control::Reveal));
        assert_eq!(parse("toggle-autohide"), Some(Control::ToggleAutohide));
    }
}
