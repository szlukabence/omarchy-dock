//! Talking to the rest of Omarchy at runtime.
//!
//! What lives here shares a principle: where Omarchy already
//! has a way to do something, use it rather than reimplementing it.
//!
//! * **Shell surfaces.** `omarchy-shell` forwards IPC to the running shell, so
//!   the dock can open the real Omarchy menu, clipboard and emoji pickers
//!   instead of drawing lookalikes.
//! * **Notifications.** `omarchy notification send` goes through the shell's
//!   own notification service, so the dock's messages are styled and grouped
//!   like every other notification on the desktop.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Run a command without waiting for it, and without letting its output leak
/// into the dock's own log.
///
/// Every call here is advisory: Omarchy may not be installed, the shell may
/// not be running, and neither is a reason for the dock to fail. Failures are
/// logged at debug and otherwise ignored.
fn spawn(program: &str, args: &[&str]) {
    match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => tracing::debug!(program, ?args, "omarchy command"),
        Err(e) => tracing::debug!(program, ?args, error = %e, "omarchy command unavailable"),
    }
}

// ── shell surfaces ──────────────────────────────────────────────────────────

/// One of the Omarchy shell's own surfaces, openable from the dock.
///
/// Deliberately a fixed list rather than free-form commands: these are the
/// surfaces a dock has any business raising, and naming them means the menu
/// can be built without the user configuring anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The root Omarchy menu — the same one the bar's leftmost widget opens.
    Menu,
    /// Theme picker, with each theme's shipped preview.
    Themes,
    /// Wallpaper picker for the current theme.
    Backgrounds,
    Clipboard,
    Emojis,
}

impl Surface {
    pub fn label(self) -> &'static str {
        match self {
            Surface::Menu => "Omarchy menu",
            Surface::Themes => "Themes…",
            Surface::Backgrounds => "Backgrounds…",
            Surface::Clipboard => "Clipboard…",
            Surface::Emojis => "Emojis…",
        }
    }

    /// Everything the dock offers, in menu order.
    pub fn all() -> [Surface; 5] {
        [
            Surface::Menu,
            Surface::Themes,
            Surface::Backgrounds,
            Surface::Clipboard,
            Surface::Emojis,
        ]
    }
}

/// Raise a shell surface.
///
/// The menu routes go through `omarchy.menu` with a route payload, which is
/// exactly what `omarchy menu summon <route>` does — but calling the IPC
/// directly skips a shell script and a `jq` invocation per click.
pub fn open(surface: Surface) {
    match surface {
        Surface::Menu => menu_route("root"),
        Surface::Themes => menu_route("style.theme"),
        Surface::Backgrounds => menu_route("style.background"),
        Surface::Clipboard => spawn("omarchy-shell", &["-q", "shell", "toggle", "omarchy.clipboard"]),
        Surface::Emojis => spawn("omarchy-shell", &["-q", "shell", "toggle", "omarchy.emojis"]),
    }
}

fn menu_route(route: &str) {
    // The payload is small and fully controlled here, so it is built by hand
    // rather than pulling in a serializer for five string literals.
    let payload = format!("{{\"menu\":\"{route}\"}}");
    spawn("omarchy-shell", &["-q", "shell", "summon", "omarchy.menu", &payload]);
}

// ── do not disturb ──────────────────────────────────────────────────────────

/// Whether the shell's Do Not Disturb is on, or `None` if the shell cannot be
/// asked.
///
/// Queried when a menu opens rather than tracked: DND can be flipped from the
/// bar or a keybinding too, the shell offers no change signal, and polling a
/// setting nobody is looking at would be waste. The query takes ~30 ms.
pub fn dnd_enabled() -> Option<bool> {
    let out = Command::new("omarchy-shell")
        .args(["notifications", "isDnd"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "on" | "true" => Some(true),
        "off" | "false" => Some(false),
        _ => None,
    }
}

/// Flip the shell's Do Not Disturb.
pub fn toggle_dnd() {
    spawn("omarchy-shell", &["-q", "notifications", "toggleDnd"]);
}

// ── notifications ───────────────────────────────────────────────────────────

/// Send a desktop notification through Omarchy's own notification service.
///
/// Using Omarchy's command rather than libnotify means the dock's messages get
/// the shell's styling, its glyph column and its notification centre, instead
/// of looking like they came from somewhere else.
pub fn notify(headline: &str, body: Option<&str>, glyph: Option<&str>) {
    let mut args: Vec<&str> = vec!["notification", "send", "--app-name", "Dock"];
    if let Some(g) = glyph {
        args.push("-g");
        args.push(g);
    }
    args.push(headline);
    if let Some(b) = body {
        args.push(b);
    }
    spawn("omarchy", &args);
}

// ── screen recording ────────────────────────────────────────────────────────

/// Prefix of every file `omarchy-capture-screenrecording` keeps in
/// `$XDG_RUNTIME_DIR` (`-filename`, `-pid`, `-region`, `-starting`, …).
const RECORDER_FILE_PREFIX: &str = "omarchy-screenrecord-";

/// How long a burst of recorder-file changes must be quiet before asking.
const RECORDING_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// The runtime directory Omarchy's recorder keeps its files in. `None` when
/// `XDG_RUNTIME_DIR` is unset or not absolute, which switches recording
/// detection off: there is no fallback to `/tmp`.
pub fn runtime_dir_from(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let p = PathBuf::from(value?);
    p.is_absolute().then_some(p)
}

/// Whether `path` is one of the recorder's own files.
pub fn is_recorder_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(RECORDER_FILE_PREFIX))
}

/// `omarchy-capture-screenrecording --status` exits 0 only while a recording
/// of its own is running. A command that could not run, or was killed by a
/// signal, is not a recording.
pub fn recording_from_status(status: std::io::Result<std::process::ExitStatus>) -> bool {
    status.is_ok_and(|s| s.code() == Some(0))
}

/// Ask Omarchy whether a recording is running. A marker file is not proof
/// (a crashed recorder leaves its files behind), so the recorder's own
/// read-only `--status` is the authority. Bounded, so a wedged recorder
/// cannot stall the watcher.
pub fn is_recording() -> bool {
    let child = Command::new("omarchy-capture-screenrecording")
        .arg("--status")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return recording_from_status(Ok(status)),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                return false;
            }
        }
    }
}

/// Watch the recorder's files and report whether a recording is running, as
/// `AppEvent::Recording`: once at startup if one is, then after every change
/// that flips the answer. Runs its own thread and never touches GTK.
///
/// Changes are debounced, and `--status` runs on this one thread, so there is
/// never more than one at a time; a change arriving while one runs is queued
/// and answered by the next run.
pub fn spawn_recording_watcher(tx: crate::event::Sender) {
    let Some(dir) = runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR")) else {
        tracing::info!("XDG_RUNTIME_DIR unset: screen-recording detection is off");
        return;
    };
    let spawned = std::thread::Builder::new().name("omarchy-dock-recording".into()).spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (raw_tx, raw_rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(raw_tx) {
            Ok(w) => w,
            Err(e) => return tracing::warn!(error = %e, "cannot watch for screen recordings"),
        };
        // Watched before the first check, so a change in between is not lost.
        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            return tracing::warn!(error = %e, "cannot watch for screen recordings");
        }
        let mut last = is_recording();
        if last && tx.send_blocking(crate::event::AppEvent::Recording(true)).is_err() {
            return;
        }
        let relevant = |ev: notify::Result<notify::Event>| {
            ev.is_ok_and(|e| !e.kind.is_access() && e.paths.iter().any(|p| is_recorder_file(p)))
        };
        while let Ok(ev) = raw_rx.recv() {
            if !relevant(ev) {
                continue;
            }
            // Wait for the burst to end, however long it runs.
            while raw_rx.recv_timeout(RECORDING_DEBOUNCE).is_ok() {}
            let now = is_recording();
            if now != last {
                last = now;
                if tx.send_blocking(crate::event::AppEvent::Recording(now)).is_err() {
                    return;
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch for screen recordings");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_surface_has_a_label_and_is_offered() {
        // A surface added to the enum but left out of `all()` would never be
        // reachable from the menu.
        assert_eq!(Surface::all().len(), 5);
        for s in Surface::all() {
            assert!(!s.label().is_empty());
        }
    }

    #[test]
    fn recorder_files_are_recognised_by_name() {
        use std::path::Path;
        for n in ["filename", "pid", "region", "starting", "cancel", "pa-modules", "preview-1.png"] {
            assert!(is_recorder_file(Path::new(&format!("/run/user/1000/omarchy-screenrecord-{n}"))), "{n}");
        }
        assert!(!is_recorder_file(Path::new("/run/user/1000/omarchy-screenrecord")));
        assert!(!is_recorder_file(Path::new("/run/user/1000/bus")));
        assert!(!is_recorder_file(Path::new("/run/user/1000/")));
    }

    #[test]
    fn runtime_dir_must_be_set_and_absolute() {
        assert_eq!(runtime_dir_from(Some("/run/user/1000".into())), Some(PathBuf::from("/run/user/1000")));
        assert_eq!(runtime_dir_from(None), None);
        assert_eq!(runtime_dir_from(Some("tmp".into())), None);
        assert_eq!(runtime_dir_from(Some("".into())), None);
    }

    #[test]
    fn only_exit_zero_means_recording() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        assert!(recording_from_status(Ok(ExitStatus::from_raw(0))));
        assert!(!recording_from_status(Ok(ExitStatus::from_raw(1 << 8))));
        // Killed by a signal: no exit code at all.
        assert!(!recording_from_status(Ok(ExitStatus::from_raw(9))));
        assert!(!recording_from_status(Err(std::io::Error::from(std::io::ErrorKind::NotFound))));
    }
}
