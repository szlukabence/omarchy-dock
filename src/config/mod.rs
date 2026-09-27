//! User configuration: `~/.config/omarchy-dock/config.toml`.
//!
//! Everything is optional. A missing file is written from defaults on first
//! run, importing pinned apps from Omarchy's own `dock.json` so switching from
//! the stock dock does not mean retyping the dock.
//!
//! Unknown keys are ignored rather than rejected, so a config written by a
//! newer build still loads on an older one.

pub mod watcher;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the dock sits on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

impl Position {
    /// Docks on a vertical edge stack their items top-to-bottom.
    pub fn is_vertical(self) -> bool {
        matches!(self, Position::Left | Position::Right)
    }
}

/// Which monitors get a dock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorMode {
    /// An independent dock instance on every connected output.
    #[default]
    All,
    /// Only the output named in `monitors.primary`, or Hyprland's first.
    Primary,
    /// A single dock that follows the focused output.
    Focused,
}

/// How closely the dock's chrome follows Omarchy's own surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Draw the dock the way the Omarchy shell draws the bar, menus and
    /// notifications: the theme's `shell.toml` tokens, opaque, square-ish,
    /// with the Hyprland border gradient as a hairline. The dock reads as part
    /// of the desktop rather than as a visitor from another one.
    #[default]
    Omarchy,
    /// The translucent, heavily rounded, compositor-blurred slab. Not what
    /// Omarchy looks like, but it is what a dock traditionally looks like.
    Glass,
}

/// What hovering an icon does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hover {
    /// Magnify the hovered icon. Distinctive, but nothing else in Omarchy
    /// changes size on hover.
    Scale,
    /// Paint the shell's own hover fill behind the icon — the same treatment
    /// every bar widget and menu row uses.
    #[default]
    Fill,
    /// No hover treatment beyond the name label.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HideMode {
    /// Always visible.
    Never,
    /// Always hidden until the pointer hits the trigger edge.
    Always,
    /// Hidden only while a window would overlap the dock's rectangle. The
    /// default: unobtrusive when the screen is busy, present when it is not.
    #[default]
    Intelligent,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub dock: Dock,
    pub magnify: Magnify,
    pub autohide: Autohide,
    pub theme: Theme,
    pub launcher: Launcher,
    pub monitors: Monitors,
    pub items: Items,
    pub workspaces: Workspaces,
    pub tray: Tray,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Dock {
    pub position: Position,
    /// Gap between the dock and the screen edge, in logical pixels.
    pub edge_offset: i32,
    pub icon_size: f64,
    pub padding_x: f64,
    pub padding_y: f64,
    /// Gap between slots. `None` derives it from `magnify.zoom` so a magnified
    /// icon never overlaps its neighbours.
    pub spacing: Option<f64>,
    pub radius: f64,
    /// Reserve screen space so windows never sit under the dock.
    pub reserve_space: bool,
    /// Offset the dock past the Omarchy bar when both are on the same screen
    /// edge, instead of sitting underneath it.
    pub avoid_bar: bool,
    /// How long the pointer must rest on an icon before its name appears.
    pub tooltip_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Magnify {
    pub enabled: bool,
    /// What hovering does. `Scale` is the classic dock magnification;
    /// `Fill` matches the rest of Omarchy.
    pub hover: Hover,
    /// Scale factor of the hovered icon. Only the hovered icon scales.
    pub zoom: f64,
    /// Extra upward travel at full zoom, on top of bottom-anchored scaling.
    pub lift: f64,
    /// Spring constant. Higher is snappier.
    pub stiffness: f64,
    /// Fraction of critical damping. 1.0 never overshoots; below ~0.9 gives
    /// the slight pop that reads as "alive" rather than "sliding".
    pub damping_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Autohide {
    pub mode: HideMode,
    /// How long the pointer must rest on the trigger edge before revealing.
    pub reveal_delay_ms: u64,
    /// Grace period after the pointer leaves before hiding again.
    pub hide_delay_ms: u64,
    /// Thickness of the screen-edge trigger strip, in pixels.
    pub trigger_px: i32,
    /// Duration of the slide in/out animation.
    pub slide_ms: u64,
    /// Get out of the way of a fullscreen window, whatever `mode` says. A dock
    /// floating over a fullscreen video is never what was wanted.
    pub hide_on_fullscreen: bool,
    /// Get out of the way of `omarchy capture screenrecording`, so the dock
    /// does not end up in the recording.
    pub hide_while_recording: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    /// Track the active Omarchy theme and restyle live when it changes.
    pub follow_omarchy: bool,
    /// Whether the dock draws itself as an Omarchy surface or as glass.
    pub style: Style,
    /// Take sizes from the theme's `shell.toml` scale, so the dock grows and
    /// shrinks with the bar when `omarchy display text size` changes.
    pub follow_shell_scale: bool,
    /// Corner radius. `None` mirrors Hyprland's `decoration:rounding`, which
    /// is what the Omarchy shell does for every surface it draws.
    pub radius: Option<f64>,
    /// Alpha of the glass panel. Ignored by the Omarchy style, which takes its
    /// opacity from the theme's `[popups] background-alpha`.
    pub opacity: f64,
    /// Extra CSS layered over the generated stylesheet, reloaded on save.
    pub user_css: PathBuf,
    /// Icon theme override. Empty follows the Omarchy theme's `icons.theme`.
    pub icon_theme: String,
    /// Searched before the system icon theme.
    pub icon_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Launcher {
    pub enabled: bool,
    pub icon: String,
    /// Shell command to run. Empty uses Omarchy's own menu.
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Monitors {
    pub mode: MonitorMode,
    /// Output name for `MonitorMode::Primary`, e.g. "eDP-1".
    pub primary: String,
}

/// The system tray, hosted in the dock rather than the bar.
///
/// Off by default, for the same reason the workspace strip is: Omarchy's bar
/// already has a tray widget, and showing every item twice is not an
/// improvement. Turn this on and `omarchy.tray` off to move it here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Tray {
    pub enabled: bool,
    /// Show items whose status is "Passive". Applications use it to mean
    /// "nothing to see", and most trays hide them.
    pub show_passive: bool,
}

impl Default for Tray {
    fn default() -> Self {
        Self { enabled: false, show_passive: true }
    }
}

/// The workspace strip, which mirrors what the bar's workspace widget shows.
///
/// Off by default: Omarchy's bar already has one, and adding a second
/// unasked would be duplication rather than integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspaces {
    pub enabled: bool,
    /// Show workspaces with no windows on them. With this off, only occupied
    /// workspaces and the current one get a tile.
    pub show_empty: bool,
    /// A tile for Omarchy's `special:scratchpad`, showing how many windows are
    /// stashed in it.
    pub scratchpad: bool,
}

impl Default for Workspaces {
    fn default() -> Self {
        Self { enabled: false, show_empty: true, scratchpad: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Items {
    /// Desktop-entry ids or Hyprland window classes, in dock order.
    pub pinned: Vec<String>,
    pub folders: Vec<Folder>,
    pub show_trash: bool,
    /// Shell commands that can be pinned as tiles, referenced from `pinned`
    /// as `cmd:<id>`.
    pub commands: Vec<CommandItem>,
    /// Show apps that are running but not pinned.
    pub show_running: bool,
    /// Draw the dock's own furniture — launcher, folders, Trash — as
    /// monochrome glyphs in the theme foreground, the way every Omarchy bar
    /// widget is drawn, so only real application icons carry colour.
    pub glyph_ui: bool,
}

/// A pinned shell command, drawn as a glyph rather than an application icon.
///
/// This is how the Omarchy menu defines its own rows — a Nerd Font glyph, a
/// label and a command — so anything reachable from the menu or the `omarchy`
/// CLI can become a dock tile without a `.desktop` file existing for it.
///
/// Referenced from `items.pinned` as `cmd:<id>`, which keeps commands in the
/// same ordered list as apps so they drag and reorder like everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandItem {
    pub id: String,
    pub label: String,
    /// A glyph, e.g. "\uf07c". Falls back to a generic mark when empty.
    #[serde(default)]
    pub glyph: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub path: PathBuf,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    /// Show this stack. Each folder toggles independently.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

// ── defaults ────────────────────────────────────────────────────────────────

impl Default for Dock {
    fn default() -> Self {
        Self {
            position: Position::Bottom,
            edge_offset: 12,
            icon_size: 48.0,
            padding_x: 16.0,
            padding_y: 10.0,
            spacing: None,
            radius: 22.0,
            reserve_space: false,
            avoid_bar: true,
            tooltip_delay_ms: 400,
        }
    }
}

impl Default for Magnify {
    fn default() -> Self {
        // Values validated in the Phase-0 spike: vsync-locked at 60Hz with a
        // single dropped frame in ~315, and no overshoot ringing.
        Self {
            enabled: true,
            hover: Hover::Fill,
            zoom: 1.45,
            lift: 6.0,
            stiffness: 460.0,
            damping_ratio: 0.82,
        }
    }
}

impl Default for Autohide {
    fn default() -> Self {
        Self {
            mode: HideMode::Intelligent,
            reveal_delay_ms: 160,
            hide_delay_ms: 500,
            trigger_px: 2,
            slide_ms: 220,
            hide_on_fullscreen: true,
            hide_while_recording: true,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            follow_omarchy: true,
            style: Style::Omarchy,
            follow_shell_scale: true,
            radius: None,
            opacity: 0.55,
            user_css: config_dir().join("style.css"),
            icon_theme: String::new(),
            icon_paths: Vec::new(),
        }
    }
}

impl Default for Launcher {
    fn default() -> Self {
        Self { enabled: true, icon: "omarchy".into(), command: String::new() }
    }
}

impl Default for Monitors {
    fn default() -> Self {
        Self { mode: MonitorMode::All, primary: String::new() }
    }
}

impl Default for Items {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            folders: Vec::new(),
            show_trash: true,
            commands: Vec::new(),
            show_running: true,
            glyph_ui: true,
        }
    }
}



// ── paths ───────────────────────────────────────────────────────────────────

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("omarchy-dock")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Expand a leading `~` against the user's home directory.
///
/// Handles a bare `~` as well as `~/…`. Omarchy's own `omadock.json` pins Home
/// as exactly `"~"`, which a `~/`-only check leaves as a literal relative path.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    if s == "~" {
        return home();
    }
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => p.to_path_buf(),
    }
}


// ── loading ─────────────────────────────────────────────────────────────────

impl Config {
    /// Load the config, creating it from defaults on first run.
    ///
    /// A malformed file is a soft failure: we log and fall back to defaults
    /// rather than refusing to start, since the dock may be the user's only
    /// way to launch a text editor to fix it.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => {
                    tracing::info!(path = %path.display(), "config loaded");
                    cfg
                }
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "invalid config; using defaults");
                    // Worth a notification rather than only a log line: the
                    // dock silently reverting to defaults looks like a bug,
                    // and the dock may be the user's only way to reach an
                    // editor to fix the file.
                    crate::omarchy::notify(
                        "Dock config is invalid",
                        Some(&format!("Using defaults. {e}")),
                        Some("\u{f071}"),
                    );
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::bootstrap();
                if let Err(e) = cfg.save() {
                    tracing::warn!(error = %e, "could not write initial config");
                }
                cfg
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "cannot read config");
                Config::default()
            }
        }
    }

    /// Load the config, apply `f`, and save it.
    ///
    /// Unlike `load`, a file that cannot be read or parsed is an error here
    /// rather than defaults: saving defaults plus one change over it would
    /// throw away everything else the user had written there. The user is
    /// told, and the file is left for them to fix.
    pub fn edit(f: impl FnOnce(&mut Config)) -> Result<()> {
        let result = Self::edit_at(&config_path(), Config::bootstrap, f);
        if let Err(e) = &result {
            crate::omarchy::notify(
                "Dock config not saved",
                Some(&format!("The change was not written, to keep the file as it is. {e:#}")),
                Some("\u{f071}"),
            );
        }
        result
    }

    /// `edit` on the file at `path`, starting from `fresh()` if there is none.
    ///
    /// Only what the change touched is written. The file is edited in place,
    /// so the user's comments, layout, and any keys this version does not know
    /// stay exactly as they were.
    fn edit_at(path: &Path, fresh: impl FnOnce() -> Config, f: impl FnOnce(&mut Config)) -> Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut cfg = fresh();
                f(&mut cfg);
                let text = toml::to_string_pretty(&cfg).context("serialising config")?;
                return crate::safe_write::replace(path, text.as_bytes());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let parsing = || format!("parsing {}", path.display());
        let before: Config = toml::from_str(&text).with_context(parsing)?;
        let mut after = before.clone();
        f(&mut after);

        let serialised = |cfg: &Config| -> Result<toml_edit::DocumentMut> {
            Ok(toml::to_string_pretty(cfg).context("serialising config")?.parse()?)
        };
        let (old, new) = (serialised(&before)?, serialised(&after)?);
        let mut doc: toml_edit::DocumentMut = text.parse().with_context(parsing)?;
        merge_changes(doc.as_table_mut(), old.as_table(), new.as_table());
        let edited = doc.to_string();

        // The edited file must mean exactly the edited config; if it somehow
        // does not, saving it would be saving something the user did not ask
        // for, so nothing is written.
        let reread: Config = toml::from_str(&edited).context("re-reading the edited config")?;
        anyhow::ensure!(
            toml::to_string(&reread)? == toml::to_string(&after)?,
            "could not apply the change to {} without rewriting it; nothing was saved",
            path.display()
        );
        crate::safe_write::replace(path, edited.as_bytes())
    }

    /// Defaults, plus anything worth importing from the stock Omarchy dock.
    fn bootstrap() -> Self {
        let mut cfg = Config::default();
        let (pinned, folders) = import_omadock();
        if !pinned.is_empty() {
            tracing::info!(count = pinned.len(), "imported pinned apps from omadock");
            cfg.items.pinned = pinned;
        }
        if !folders.is_empty() {
            cfg.items.folders = folders;
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        let text = toml::to_string_pretty(self).context("serialising config")?;
        crate::safe_write::replace(&path, text.as_bytes())?;
        tracing::info!(path = %path.display(), "config written");
        Ok(())
    }

    /// Slot pitch: icon plus gap. Derived from the zoom factor unless pinned,
    /// so a magnified icon never touches its neighbours.
    pub fn spacing(&self) -> f64 {
        self.dock.spacing.unwrap_or_else(|| {
            if self.magnify.enabled {
                ((self.magnify.zoom - 1.0) * self.dock.icon_size).max(10.0) + 2.0
            } else {
                12.0
            }
        })
    }

    /// Absolute damping coefficient implied by `damping_ratio`.
    pub fn damping(&self) -> f64 {
        2.0 * self.magnify.stiffness.sqrt() * self.magnify.damping_ratio
    }

    /// What the launcher button runs. Defaults to Omarchy's own menu.
    pub fn launcher_command(&self) -> String {
        if self.launcher.command.trim().is_empty() {
            "omarchy-menu toggle".to_string()
        } else {
            self.launcher.command.clone()
        }
    }

    /// Vertical space a hovered icon's name label occupies.
    ///
    /// Reserved inside the surface rather than shown in a popover: a
    /// non-autohide popover is drawn within its parent surface's bounds, so a
    /// label would be clipped by the dock's own edge, and an autohide one
    /// would take a pointer grab and fight the hover that summoned it.
    pub const LABEL_BAND: f64 = 26.0;

    /// Headroom beyond the panel: room for a magnified icon to grow into, plus
    /// the name label above it.
    pub fn headroom(&self) -> f64 {
        let zoom = if self.magnify.enabled {
            (self.magnify.zoom - 1.0) * self.dock.icon_size + self.magnify.lift + 4.0
        } else {
            0.0
        };
        zoom + Self::LABEL_BAND
    }
}

/// Apply to `doc` — the user's file — what changed between `old` and `new`,
/// both the config as the dock serialises it. A key whose value is unchanged
/// is not touched, so it keeps its formatting and its comments, and keys the
/// dock does not know are never looked at.
fn merge_changes(doc: &mut dyn toml_edit::TableLike, old: &dyn toml_edit::TableLike, new: &dyn toml_edit::TableLike) {
    use toml_edit::Item;
    for (key, new_item) in new.iter() {
        let old_item = old.get(key);
        let tables = match (new_item, old_item) {
            (Item::Table(n), Some(Item::Table(o))) => Some((n, o)),
            _ => None,
        };
        match tables {
            // A section: descend, so only the keys that changed are written.
            Some((new_table, old_table)) => {
                if doc.get(key).is_none() {
                    let mut table = toml_edit::Table::new();
                    table.set_implicit(true);
                    doc.insert(key, Item::Table(table));
                }
                match doc.get_mut(key).and_then(Item::as_table_like_mut) {
                    Some(sub) => merge_changes(sub, old_table, new_table),
                    None => {
                        doc.insert(key, new_item.clone());
                    }
                }
            }
            None if old_item.is_some_and(|o| rendered(o) == rendered(new_item)) => {}
            None => match doc.get_mut(key) {
                // Keep the key's own decoration (its comments) where it has one.
                Some(Item::Value(v)) if new_item.is_value() => {
                    let decor = v.decor().clone();
                    *v = new_item.as_value().cloned().expect("checked is_value");
                    *v.decor_mut() = decor;
                }
                _ => {
                    doc.insert(key, new_item.clone());
                }
            },
        }
    }
    let gone: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| new.get(k).is_none())
        .collect();
    for key in gone {
        doc.remove(&key);
    }
}

/// An item as TOML text, for telling whether two are the same.
fn rendered(item: &toml_edit::Item) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    doc.insert("v", item.clone());
    doc.to_string()
}

// ── migration from the stock dock ───────────────────────────────────────────

/// Read pinned apps from `~/.config/omarchy/dock.json` and pinned folders from
/// `omadock.json`, so a first run inherits the user's existing dock.
fn import_omadock() -> (Vec<String>, Vec<Folder>) {
    let base = dirs::config_dir().unwrap_or_default().join("omarchy");

    let pinned = std::fs::read_to_string(base.join("dock.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("pinned")?
                .as_array()?
                .iter()
                .map(|s| s.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();

    let folders = std::fs::read_to_string(base.join("omadock.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            Some(
                v.get("pinnedFolders")?
                    .as_array()?
                    .iter()
                    .filter_map(|f| {
                        Some(Folder {
                            path: PathBuf::from(f.get("path")?.as_str()?),
                            name: f.get("name")?.as_str()?.to_owned(),
                            icon: f
                                .get("icon")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            enabled: true,
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or_default();

    (pinned, folders)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_bare_tilde_as_well_as_tilde_slash() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("~/Downloads")), home.join("Downloads"));
        // Only a leading ~ is special; a path containing one is left alone.
        assert_eq!(expand_tilde(Path::new("/tmp/~x")), PathBuf::from("/tmp/~x"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("omarchy-dock-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn an_edit_never_saves_over_a_file_it_could_not_parse() {
        let path = scratch("invalid");
        let text = "[dock]\nicon_size = \"big\"   # a typo, and a comment worth keeping\n";
        std::fs::write(&path, text).unwrap();
        let edited = Config::edit_at(&path, Config::default, |c| c.items.pinned.push("x".into()));
        assert!(edited.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn an_edit_keeps_the_rest_of_the_file() {
        let path = scratch("valid");
        let mut cfg = Config::default();
        cfg.dock.icon_size = 61.0;
        std::fs::write(&path, toml::to_string_pretty(&cfg).unwrap()).unwrap();
        Config::edit_at(&path, Config::default, |c| c.items.pinned.push("x".into())).unwrap();
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.dock.icon_size, 61.0);
        assert_eq!(saved.items.pinned.last().map(String::as_str), Some("x"));
    }

    #[test]
    fn an_edit_keeps_comments_layout_and_unknown_keys() {
        let path = scratch("comments");
        let text = "# my dock\n\
                    [dock]\n\
                    icon_size = 40   # big enough\n\
                    some_future_key = \"kept\"\n\
                    \n\
                    [items]\n\
                    # the apps I use\n\
                    pinned = [\"a\", \"b\"]\n";
        std::fs::write(&path, text).unwrap();
        Config::edit_at(&path, Config::default, |c| {
            c.items.pinned.push("x".into());
            c.autohide.mode = HideMode::Never;
        })
        .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        for kept in ["# my dock", "icon_size = 40   # big enough", "some_future_key = \"kept\"", "# the apps I use"] {
            assert!(saved.contains(kept), "lost {kept:?}:\n{saved}");
        }
        let cfg: Config = toml::from_str(&saved).unwrap();
        assert_eq!(cfg.items.pinned, vec!["a", "b", "x"]);
        assert_eq!(cfg.dock.icon_size, 40.0);
        assert_eq!(cfg.autohide.mode, HideMode::Never);
        // Nothing the edit did not touch was written out.
        assert!(!saved.contains("[magnify]"), "{saved}");
    }

    #[test]
    fn an_edit_reaches_into_an_inline_table() {
        let path = scratch("inline");
        std::fs::write(&path, "dock = { icon_size = 40 }  # inline\n").unwrap();
        Config::edit_at(&path, Config::default, |c| c.dock.icon_size = 52.0).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# inline"), "{saved}");
        let cfg: Config = toml::from_str(&saved).unwrap();
        assert_eq!(cfg.dock.icon_size, 52.0);
    }

    #[test]
    fn an_edit_replaces_lists_and_clears_what_was_unset() {
        let path = scratch("lists");
        let text = "[dock]\nspacing = 12.0 # tight\n\n\
                    [[items.folders]]\npath = \"/a\"\nname = \"A\"\n\n\
                    [[items.folders]]\npath = \"/b\"\nname = \"B\"\n";
        std::fs::write(&path, text).unwrap();
        Config::edit_at(&path, Config::default, |c| {
            c.dock.spacing = None;
            c.items.folders.remove(0);
        })
        .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&saved).unwrap();
        assert_eq!(cfg.dock.spacing, None, "{saved}");
        assert_eq!(cfg.items.folders.len(), 1);
        assert_eq!(cfg.items.folders[0].path, PathBuf::from("/b"));
    }

    #[test]
    fn a_first_edit_starts_from_fresh() {
        let path = scratch("fresh");
        Config::edit_at(&path, Config::default, |c| c.items.pinned = vec!["x".into()]).unwrap();
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.items.pinned, vec!["x".to_string()]);
    }
}
