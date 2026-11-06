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
    /// Focus, minimize, or launch the nth dock item (1-based, as typed by users).
    Activate(usize),
    /// Minimize the focused window.
    Minimize,
    /// Restore the most recently focused minimized window, of any app.
    Restore,
    /// Put every minimized window back where it came from, as before
    /// uninstalling.
    RestoreAll,
    /// Force the dock visible, hidden, or back to its configured behaviour.
    Reveal,
    Hide,
    ToggleAutohide,
    /// Re-read config and rebuild.
    Reload,
    /// Re-read the active Omarchy theme and restyle, without rebuilding.
    /// What Omarchy's `theme-set` hook fires.
    Restyle,
    /// Open the settings window.
    Settings,
}

/// The control socket, in the user's private runtime directory.
///
/// Never in `/tmp` as a fallback: that is shared by every user, and anyone who
/// could reach the socket could drive the dock — `activate` launches apps. A
/// session without `XDG_RUNTIME_DIR` (never the case under Hyprland) simply
/// has no control socket.
pub fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|b| !b.is_empty())?;
    Some(PathBuf::from(base).join("omarchy-dock.sock"))
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
        "minimize" => Some(Control::Minimize),
        "restore" => match parts.next() {
            None => Some(Control::Restore),
            Some("--all") => Some(Control::RestoreAll),
            Some(_) => None,
        },
        "reveal" => Some(Control::Reveal),
        "hide" => Some(Control::Hide),
        "toggle-autohide" => Some(Control::ToggleAutohide),
        "reload" => Some(Control::Reload),
        "restyle" => Some(Control::Restyle),
        "settings" => Some(Control::Settings),
        _ => None,
    }
}

/// Whether a process named `comm` is a dock from before the program was
/// renamed `berth`: Omarchy 4.5 ships a command of its own at the old name.
fn predates_rename(comm: &str) -> bool {
    comm.trim_end() == "omarchy-dock"
}

/// Stop a dock from before the rename, if one is running, before this one
/// starts.
///
/// Upgrading leaves that dock running, and nothing at the new name reaches
/// it: `pkill -x berth` misses it, `pkill -x omarchy-dock` would also hit
/// Omarchy's own command of that name, and GTK hands this launch over to it
/// as the app's running instance and exits. Whatever holds the control
/// socket is a dock, so its process is ours to stop. This waits for it to be
/// gone: on the way out it gives back the bar's workspaces, which this one
/// would otherwise race.
pub fn replace_older_dock() {
    let Some(path) = socket_path() else { return };
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else { return };
    let Some(pid) = peer_pid(&stream) else { return };
    drop(stream);
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    if !predates_rename(&comm) {
        return;
    }
    let gone = |tries: u32| {
        (0..tries).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            exited(std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().as_deref())
        })
    };
    // It may be on its way out already, told by the plugin it was swapped
    // for: a second signal makes it quit without giving back the bar's
    // workspaces.
    if gone(10) {
        return;
    }
    tracing::info!(pid, "stopping the dock this one replaces");
    // SAFETY: kill(2) only sends a signal; the pid is the socket's peer.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    if !gone(100) {
        tracing::warn!(pid, "the dock this one replaces is still running");
    }
}

/// The process at the other end of a Unix socket, if it is the user's own.
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` describe a buffer of exactly a `ucred`.
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } == 0;
    // The runtime directory is the user's own; this is only in case.
    // SAFETY: getuid(2) cannot fail.
    let mine = cred.uid == unsafe { libc::getuid() };
    (ok && mine && cred.pid > 0).then_some(cred.pid)
}

/// Whether a process has exited, from its `/proc/<pid>/stat`: gone, or a
/// zombie waiting for its parent.
fn exited(stat: Option<&str>) -> bool {
    let Some(stat) = stat else { return true };
    // The state follows the name, which is in brackets and may hold either.
    let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next());
    matches!(state, Some("Z" | "X"))
}

/// Serve the control socket until the process exits.
pub async fn serve(tx: Sender) -> Result<()> {
    let path = socket_path().context("XDG_RUNTIME_DIR is not set; no control socket")?;

    // A stale socket from a crashed instance would make bind() fail. Removing
    // it is safe: a live instance would still be holding the path, and we
    // check for that first by trying to connect.
    //
    // A dock this one replaced may still be on its way out, so it is given a
    // few seconds to let go.
    if path.exists() {
        let mut tries = 0;
        while UnixStream::connect(&path).await.is_ok() {
            tries += 1;
            if tries == 50 {
                anyhow::bail!("another dock is already running at {}", path.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
    fn only_a_dock_from_before_the_rename_is_replaced() {
        // What /proc/<pid>/comm reads, newline and all.
        assert!(predates_rename("omarchy-dock\n"));
        // A second berth is not replaced: two docks would fight.
        assert!(!predates_rename("berth\n"));
        assert!(!predates_rename("omarchy-dockctl\n"));
    }

    #[test]
    fn a_process_that_is_gone_or_a_zombie_has_exited() {
        assert!(exited(None));
        assert!(exited(Some("4321 (omarchy-dock) Z 1 4321 4321 0 -1")));
        // The name is in brackets and may hold spaces or brackets itself.
        assert!(!exited(Some("4321 (a) b) S 1 4321 4321 0 -1")));
        assert!(!exited(Some("4321 (omarchy-dock) S 1 4321 4321 0 -1")));
    }

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

    #[test]
    fn minimize_and_restore_are_verbs() {
        assert_eq!(parse("minimize"), Some(Control::Minimize));
        assert_eq!(parse("restore"), Some(Control::Restore));
        assert_eq!(parse("restore --all"), Some(Control::RestoreAll));
        assert_eq!(parse("restore --some"), None);
    }
}
