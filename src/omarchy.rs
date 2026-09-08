//! Talking to the rest of Omarchy at runtime.
//!
//! Three things live here, and they share a principle: where Omarchy already
//! has a way to do something, use it rather than reimplementing it.
//!
//! * **Shell surfaces.** `omarchy-shell` forwards IPC to the running shell, so
//!   the dock can open the real Omarchy menu, clipboard and emoji pickers
//!   instead of drawing lookalikes.
//! * **Notifications.** `omarchy notification send` goes through the shell's
//!   own notification service, so the dock's messages are styled and grouped
//!   like every other notification on the desktop.
//! * **The bar.** `shell.json` says where the bar is. A dock that ignored it
//!   would happily park itself on top of the bar.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::Position;

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

/// Marker Omarchy's screen recorder writes while a recording is running.
///
/// `omarchy capture screenrecording` creates it on start and deletes it on
/// stop, so its existence is the cheapest correct answer to "is the screen
/// being recorded" — no polling, and a plain file the dock can watch.
pub static RECORDING_MARKER: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| PathBuf::from("/tmp/omarchy-screenrecord-filename"));

/// Whether a screen recording is in progress.
pub fn is_recording() -> bool {
    RECORDING_MARKER.exists()
}

// ── the bar ─────────────────────────────────────────────────────────────────

/// Where the Omarchy bar is and how thick it is, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub position: Position,
    pub thickness: f64,
}

fn shell_json_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omarchy/shell.json")
}

/// Read the bar's placement from `shell.json` and its thickness from the
/// theme's `[bar]` tokens.
///
/// Returns `None` when there is no Omarchy shell config to read, which is the
/// "there is no bar to avoid" case.
pub fn bar(shell: &crate::theme::shell::Shell) -> Option<Bar> {
    let text = std::fs::read_to_string(shell_json_path()).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let position = match json.get("bar")?.get("position")?.as_str()? {
        "bottom" => Position::Bottom,
        "left" => Position::Left,
        "right" => Position::Right,
        _ => Position::Top,
    };

    // The bar's cross-axis size is a `shell.toml` token, and it scales with
    // the font the same way the dock now does.
    let base = if position.is_vertical() {
        shell.bar.size_vertical
    } else {
        shell.bar.size_horizontal
    };
    let thickness = if shell.bar.scale_with_font {
        base * shell.metrics.font_scale()
    } else {
        base
    };

    let bar = Bar { position, thickness: thickness.round() };
    tracing::debug!(?bar, "omarchy bar");
    Some(bar)
}

/// Extra gap the dock needs so it does not sit underneath the bar.
///
/// Only matters when both are on the same screen edge: elsewhere they cannot
/// overlap. Stacking the dock above the bar is the least surprising answer —
/// moving the dock to another edge would silently override a position the user
/// chose, and doing nothing would hide the dock behind the bar.
pub fn bar_clearance(dock: Position, bar: Option<Bar>) -> f64 {
    match bar {
        Some(b) if b.position == dock => b.thickness,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_at(position: Position) -> Bar {
        Bar { position, thickness: 26.0 }
    }

    #[test]
    fn the_dock_clears_a_bar_on_its_own_edge() {
        assert_eq!(bar_clearance(Position::Top, Some(bar_at(Position::Top))), 26.0);
    }

    #[test]
    fn a_bar_on_another_edge_needs_no_clearance() {
        // The common case: bar on top, dock on the bottom.
        assert_eq!(bar_clearance(Position::Bottom, Some(bar_at(Position::Top))), 0.0);
        assert_eq!(bar_clearance(Position::Bottom, Some(bar_at(Position::Left))), 0.0);
    }

    #[test]
    fn no_bar_means_no_clearance() {
        assert_eq!(bar_clearance(Position::Bottom, None), 0.0);
    }

    #[test]
    fn every_surface_has_a_label_and_is_offered() {
        // A surface added to the enum but left out of `all()` would never be
        // reachable from the menu.
        assert_eq!(Surface::all().len(), 5);
        for s in Surface::all() {
            assert!(!s.label().is_empty());
        }
    }
}
