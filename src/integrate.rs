//! Making the dock a first-class Omarchy citizen.
//!
//! Omarchy has real extension points, and a dock that ignores them is a
//! program that merely runs on the desktop rather than one that belongs to it.
//! Three are wired up here, all installed and removed together by
//! `omarchy-dockctl install` / `uninstall`:
//!
//! * a `theme-set` **hook**, so a theme change reaches the dock the moment
//!   Omarchy has finished writing the theme, rather than whenever an inotify
//!   watch happens to fire mid-write;
//! * a shell **plugin**, so the dock appears in `omarchy menu plugin` and the
//!   plugin managers alongside every other component, and can be started,
//!   stopped and disabled the same way;
//! * a **menu extension**, so the dock's own settings live where every other
//!   Omarchy setting lives, reachable from the launcher search.
//!
//! Everything written here is confined to `~/.config/omarchy/` — plus copies
//! of what was written in `~/.local/state/omarchy-dock/`, and the opt-in blur
//! block in `~/.config/hypr/looknfeel.lua` — is marked as belonging to the
//! dock, and is removed cleanly. The menu extension is the
//! one file we share with the user, so it is edited between markers and never
//! rewritten wholesale. Nothing is ever deleted on the strength of its name
//! alone: a plugin file goes only if it is byte for byte what the dock wrote,
//! and a plugin directory only once that leaves it empty.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Plugin id, and the directory name under `~/.config/omarchy/plugins/`.
///
/// Namespaced, as the plugin marketplace asks: listing ids are permanent and
/// global, and a bare `omarchy-dock` would read as a first-party component.
/// `omarchy plugin add` clones into a directory named after this id.
pub const PLUGIN_ID: &str = "io.github.szlukabence.omarchy-dock";

/// The id the plugin had before it was namespaced. Installs made then left a
/// plugin directory and a `shell.json` entry under it, which would otherwise
/// sit alongside the new one as a second supervisor for the same dock.
const LEGACY_PLUGIN_ID: &str = "omarchy-dock";

/// Basename of the hook we drop into `theme-set.d`.
const HOOK_NAME: &str = "omarchy-dock";

/// Fences around the block we own inside the user's menu extension file.
const MENU_BEGIN: &str = "  // >>> omarchy-dock — managed block, edits are overwritten >>>";
const MENU_END: &str = "  // <<< omarchy-dock <<<";

/// Where the dock's marker-fenced block sits in a file it shares with the user.
enum Block<'a> {
    /// No block: the text holds no start marker.
    Absent,
    /// The text before the block and after it.
    Found(&'a str, &'a str),
    /// A start marker with no end marker after it. Everything that follows
    /// would otherwise be taken for the block and lost, so the file is left
    /// alone.
    Unterminated,
}

fn find_block<'a>(text: &'a str, begin: &str, end: &str) -> Block<'a> {
    let Some((before, rest)) = text.split_once(begin) else { return Block::Absent };
    match rest.split_once(end) {
        Some((_, after)) => Block::Found(before, after),
        None => Block::Unterminated,
    }
}

/// `~/.config`. [`preflight`] has checked it can be found before anything
/// is written; there is no fallback, so nothing ever lands relative to
/// wherever the command happened to be run.
fn config_home() -> PathBuf {
    dirs::config_dir().expect("preflight found the config directory")
}

pub fn omarchy_config_dir() -> PathBuf {
    config_home().join("omarchy")
}

fn hook_path() -> PathBuf {
    omarchy_config_dir().join("hooks/theme-set.d").join(HOOK_NAME)
}

fn plugin_dir() -> PathBuf {
    omarchy_config_dir().join("plugins").join(PLUGIN_ID)
}

fn legacy_plugin_dir() -> PathBuf {
    omarchy_config_dir().join("plugins").join(LEGACY_PLUGIN_ID)
}

/// Copies of the plugin files exactly as the dock last wrote them.
///
/// Kept outside the plugin directory, where neither the shell nor the user
/// looks, so that removal can tell the dock's own files from anything else in
/// that directory — including the dock's files after someone edited them.
fn written_copies_dir() -> PathBuf {
    dirs::state_dir()
        .expect("preflight found the state directory")
        .join("omarchy-dock/plugin-files")
}

fn menu_extension_path() -> PathBuf {
    omarchy_config_dir().join("extensions/omarchy-menu.jsonc")
}

/// What one piece of integration is called and where it lives, for reporting.
pub struct Report {
    pub label: &'static str,
    pub path: PathBuf,
    pub installed: bool,
    pub note: Option<String>,
}

// ── theme-set hook ──────────────────────────────────────────────────────────

/// Omarchy runs every executable in `theme-set.d` after applying a theme.
///
/// This is strictly better than watching the theme symlink: the hook fires
/// once, after all of `colors.toml`, `shell.toml` and `icons.theme` are in
/// place, so the dock can never restyle from a half-written theme. The watcher
/// stays as a fallback for anyone who has not installed the hook.
fn hook_script() -> String {
    // Not `exec ... || true`: exec replaces the shell, so the `|| true` never
    // runs and a missing omarchy-dockctl exits 127. Omarchy reports a failing
    // hook, which would turn every theme change into an error message for
    // anyone who has the hook installed but not the dock on PATH.
    "#!/usr/bin/env bash\n\
     # Installed by omarchy-dock. Removed by `omarchy-dockctl uninstall`.\n\
     #\n\
     # Omarchy runs this after a theme is fully applied, which is the only\n\
     # moment every theme file is guaranteed to be written. Restyling here\n\
     # rather than on an inotify event means the dock can never pick up a\n\
     # half-written theme.\n\
     #\n\
     # Best-effort throughout: the dock may not be installed or not running,\n\
     # and neither may make a theme change fail.\n\
     if command -v omarchy-dockctl >/dev/null 2>&1; then\n\
     \x20 omarchy-dockctl restyle >/dev/null 2>&1 || true\n\
     fi\n\
     exit 0\n"
        .into()
}

// ── shell plugin ────────────────────────────────────────────────────────────

/// The dock is a separate process, not QML, so the plugin is a supervisor.
///
/// That is enough to make it a real Omarchy component: enabling it starts the
/// dock, disabling it stops the dock, and it is listed and managed exactly
/// like every first-party plugin. It also gives the dock an autostart that
/// follows the shell's own lifecycle instead of a stray `exec-once`.
///
/// Both files are the repository's own, embedded at compile time rather than
/// written out as string literals. The repo root has to carry them anyway —
/// that is what makes `omarchy plugin add <git url>` work — and two copies of
/// a manifest is two things to forget to update.
const PLUGIN_MANIFEST: &str = include_str!("../manifest.json");
const PLUGIN_SERVICE_QML: &str = include_str!("../Service.qml");

/// A file the dock writes: its name, what this build writes, and what earlier
/// releases wrote there before the dock kept copies of its own files.
struct DockFile<'a> {
    name: &'a str,
    current: Option<&'a str>,
    released: &'a [&'a str],
}

/// The plugin directory's files.
const PLUGIN_FILES: [DockFile<'static>; 2] = [
    DockFile {
        name: "manifest.json",
        current: Some(PLUGIN_MANIFEST),
        released: &[
            include_str!("../resources/released/1.3.0/manifest.json.txt"),
            include_str!("../resources/released/1.2.6/manifest.json.txt"),
            include_str!("../resources/released/1.2.5/manifest.json.txt"),
            include_str!("../resources/released/1.2.4/manifest.json.txt"),
            include_str!("../resources/released/1.2.3/manifest.json.txt"),
            include_str!("../resources/released/1.2.2/manifest.json.txt"),
            include_str!("../resources/released/1.2.1/manifest.json.txt"),
        ],
    },
    DockFile {
        name: "Service.qml",
        current: Some(PLUGIN_SERVICE_QML),
        released: &[
            include_str!("../resources/released/1.3.0/Service.qml.txt"),
            include_str!("../resources/released/1.2.6/Service.qml.txt"),
            include_str!("../resources/released/1.2.5/Service.qml.txt"),
            include_str!("../resources/released/1.2.4/Service.qml.txt"),
            include_str!("../resources/released/1.2.3/Service.qml.txt"),
            include_str!("../resources/released/1.2.2/Service.qml.txt"),
            include_str!("../resources/released/1.2.1/Service.qml.txt"),
        ],
    },
];

/// The pre-namespace plugin directory's files, as the last release under that
/// id wrote them. Nothing writes them any more.
const LEGACY_PLUGIN_FILES: [DockFile<'static>; 2] = [
    DockFile {
        name: "manifest.json",
        current: None,
        released: &[include_str!("../resources/released/1.2.0/manifest.json.txt")],
    },
    DockFile {
        name: "Service.qml",
        current: None,
        released: &[include_str!("../resources/released/1.2.0/Service.qml.txt")],
    },
];

/// Whether `path` holds the dock's own copy of `file`: what this build
/// writes, what a release wrote, or what an earlier build wrote and kept a
/// copy of in `copies`. A file anyone has changed since is none of these, and
/// is theirs.
fn is_dock_file(path: &Path, file: &DockFile, copies: Option<&Path>) -> bool {
    let Ok(now) = std::fs::read(path) else { return false };
    file.current.is_some_and(|c| c.as_bytes() == now)
        || file.released.iter().any(|r| r.as_bytes() == now)
        || copies.is_some_and(|c| std::fs::read(c.join(file.name)).is_ok_and(|kept| kept == now))
}

/// What in `dir` is not the dock's: every entry other than `files`, and each
/// of `files` that is there but is not the dock's. Empty when `dir` is absent.
fn foreign_entries(dir: &Path, files: &[DockFile], copies: Option<&Path>) -> Result<Vec<String>> {
    if is_symlink(dir) {
        // Linked here by someone — a development checkout, say. What it
        // points at is theirs, and so is the link.
        return Ok(vec![symlink_note(dir)]);
    }
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut foreign: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| match files.iter().find(|f| e.file_name() == f.name) {
            Some(f) => !is_dock_file(&e.path(), f, copies),
            None => true,
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    foreign.sort();
    Ok(foreign)
}

fn symlink_note(dir: &Path) -> String {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{name} (a symlink)")
}

/// Write the plugin files into `dir`, keeping a copy of each in `copies` —
/// unless `dir` holds anything that is not the dock's, in which case nothing
/// is written and what was found is returned. Overwriting is only ever
/// replacing the dock's own files with their current version.
fn write_plugin_files(dir: &Path, copies: &Path) -> Result<Vec<String>> {
    let foreign = foreign_entries(dir, &PLUGIN_FILES, Some(copies))?;
    if !foreign.is_empty() {
        return Ok(foreign);
    }
    for file in &PLUGIN_FILES {
        let contents = file.current.expect("the plugin files are current");
        write_file(&dir.join(file.name), contents)?;
        write_file(&copies.join(file.name), contents)?;
    }
    Ok(Vec::new())
}

/// Remove the dock's `files` from `dir`, then `dir` itself if that left it
/// empty. Returns the names of whatever is still there, which is not the
/// dock's to delete.
fn remove_plugin_files(dir: &Path, files: &[DockFile], copies: Option<&Path>) -> Result<Vec<String>> {
    if is_symlink(dir) {
        return Ok(vec![symlink_note(dir)]);
    }
    for file in files {
        let path = dir.join(file.name);
        if is_dock_file(&path, file, copies) {
            remove_path(&path)?;
        }
        if let Some(copies) = copies {
            remove_path(&copies.join(file.name))?;
        }
    }
    // Not remove_dir_all: this only succeeds on an empty directory.
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            foreign_entries(dir, &[], None)
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

/// Whether a git checkout at `dir` is this plugin: its manifest names our id.
/// `omarchy plugin add` clones into a directory named after the id, so any
/// other answer means someone else's repository sits at our path.
fn checkout_is_ours(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|m| m.get("id").and_then(|v| v.as_str()) == Some(PLUGIN_ID))
}

/// Enable or disable the plugin in `shell.json`.
///
/// The shell treats "enabled" as "referenced somewhere in shell.json": a bar
/// widget through the layout, everything else through a top-level `plugins[]`
/// entry. Installing the plugin files alone therefore registers it but leaves
/// it inert, which reads as the install having silently failed.
///
/// The file is plain JSON that Omarchy's own commands rewrite, so round-tripping
/// it through a serializer is how it is already maintained.
fn set_plugin_enabled(on: bool) -> Result<bool> {
    set_enabled(PLUGIN_ID, on)
}

fn set_enabled(id: &str, on: bool) -> Result<bool> {
    let path = shell_json_path();
    let Some(text) = read_existing(&path)? else {
        // No shell config at all: nothing to enable ourselves in, and creating
        // one from scratch is not the dock's business.
        return Ok(false);
    };
    let mut json: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;

    let Some(obj) = json.as_object_mut() else { return Ok(false) };
    let entries = obj
        .entry("plugins")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(list) = entries.as_array_mut() else { return Ok(false) };

    let at = list
        .iter()
        .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(id));

    let changed = match (on, at) {
        (true, None) => {
            list.push(serde_json::json!({ "id": id }));
            true
        }
        (false, Some(i)) => {
            list.remove(i);
            true
        }
        // Already in the state we want.
        _ => false,
    };

    if changed {
        let mut out = serde_json::to_string_pretty(&json)
            .context("serialising shell.json")?;
        out.push('\n');
        write_file(&path, &out)?;
    }
    Ok(changed)
}

/// What became of the pre-namespace plugin.
enum Legacy {
    /// Never installed, or already gone.
    Absent,
    /// Its files were the dock's: they are removed and its entry disabled.
    Retired,
    /// Its directory holds something the dock did not write, so it is
    /// someone else's `omarchy-dock` plugin, or the user's edit: directory
    /// and `shell.json` entry are both left as they are.
    Foreign(Vec<String>),
}

/// Retire the pre-namespace plugin, which would otherwise supervise a second
/// copy of the dock — but only when it is provably the dock's.
///
/// `omarchy-dock` is a plain name another plugin could have. So its files go
/// only if they are byte for byte what the last release under that id wrote,
/// and its `shell.json` entry only once its directory is gone: an entry for a
/// directory the dock just emptied, or one that points at nothing.
fn retire_legacy_plugin() -> Result<Legacy> {
    let dir = legacy_plugin_dir();
    let existed = dir.exists();
    if dir.join(".git").exists() {
        // A checkout is `omarchy plugin remove`'s, whoever's it is.
        return Ok(Legacy::Foreign(vec![".git".into()]));
    }
    let left = remove_plugin_files(&dir, &LEGACY_PLUGIN_FILES, None)?;
    if !left.is_empty() {
        return Ok(Legacy::Foreign(left));
    }
    let disabled = set_enabled(LEGACY_PLUGIN_ID, false)?;
    Ok(if existed || disabled { Legacy::Retired } else { Legacy::Absent })
}

fn shell_json_path() -> PathBuf {
    omarchy_config_dir().join("shell.json")
}

/// Whether `shell.json` currently references the plugin.
fn plugin_is_enabled() -> bool {
    let Ok(text) = std::fs::read_to_string(shell_json_path()) else { return false };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    json.get("plugins")
        .and_then(|p| p.as_array())
        .is_some_and(|list| {
            list.iter().any(|e| e.get("id").and_then(|v| v.as_str()) == Some(PLUGIN_ID))
        })
}

// ── menu extension ──────────────────────────────────────────────────────────

/// Rows added to the Omarchy menu.
///
/// Dotted ids give the tree its shape, so `dock.settings` lands inside `dock`
/// and `dock` lands on the root menu. Every row is a plain shell command,
/// which is the whole contract — the menu does not need to know what the dock
/// is.
fn menu_block() -> String {
    // Glyphs are Nerd Font codepoints, matching the icon column the rest of
    // the menu uses. Written as escapes so the file stays plain ASCII.
    let rows = [
        r#""dock": {"icon":"\uf07c","label":"Dock","description":"Application dock"}"#,
        r#""dock.reveal": {"icon":"\uf06e","label":"Reveal","action":"omarchy-dockctl reveal"}"#,
        r#""dock.toggle_autohide": {"icon":"\uf070","label":"Toggle auto-hide","action":"omarchy-dockctl toggle-autohide"}"#,
        r#""dock.settings": {"icon":"\uf013","label":"Settings","description":"Open the dock settings window","action":"omarchy-dockctl settings"}"#,
        r#""dock.reload": {"icon":"\uf021","label":"Reload","description":"Re-read config and rebuild","action":"omarchy-dockctl reload"}"#,
        r#""dock.restart": {"icon":"\uf01e","label":"Restart","action":"pkill -x omarchy-dock; setsid uwsm-app -- omarchy-dock >/dev/null 2>&1 &"}"#,
    ];

    let mut out = String::from(MENU_BEGIN);
    for row in rows {
        // Two spaces to match the file's own indentation. A trailing comma on
        // every row, including the last: JSONC allows it, and it means the
        // block can be removed without touching what follows.
        out.push_str("\n  ");
        out.push_str(row);
        out.push(',');
    }
    out.push('\n');
    out.push_str(MENU_END);
    out
}

/// Splice our block into the user's menu extension, between markers.
///
/// The file is shared with the user and may have anything in it, so it is
/// never rewritten: an existing block is replaced in place, and a new one is
/// inserted just before the closing brace. Returns `None` when the file has no
/// closing brace to insert before, which means it is not something we should
/// be editing — or when our start marker is there without its end marker.
pub fn splice_menu_block(existing: &str, block: &str) -> Option<String> {
    match find_block(existing, MENU_BEGIN, MENU_END) {
        // Replace whatever is currently between the markers, so reinstalling
        // after an upgrade picks up new rows instead of duplicating old ones.
        Block::Found(before, after) => return Some(format!("{before}{block}{after}")),
        Block::Unterminated => return None,
        Block::Absent => {}
    }

    // Insert before the brace that closes the object: the last `}` that is
    // JSONC content. A `}` in a comment after it, or in a string, is not it,
    // and inserting there would put the rows outside the object.
    let (at, '}') = last_significant(existing)? else { return None };
    let (before, after) = existing.split_at(at);

    // JSONC tolerates a trailing comma, but a *missing* one between our block
    // and a preceding entry would be a syntax error. Omarchy ships this file
    // as nothing but comments, so "is there an entry above us" cannot be
    // answered by looking at the last character — it has to skip comments.
    let needs_comma = last_significant_char(before).is_some_and(|c| c != ',' && c != '{');
    let sep = if needs_comma { ",\n" } else { "\n" };

    Some(format!("{}{sep}{block}\n{after}", before.trim_end()))
}

/// The last character that is actual JSONC content, ignoring comments.
///
/// String contents count: a value ending in `"` is an entry we must put a
/// comma after. Comments do not, which is the whole point — the extension file
/// Omarchy ships is an empty object wrapped in a page of examples.
fn last_significant_char(s: &str) -> Option<char> {
    last_significant(s).map(|(_, c)| c)
}

/// [`last_significant_char`], with its byte offset in `s`.
fn last_significant(s: &str) -> Option<(usize, char)> {
    let mut last = None;
    let mut chars = s.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some((i, c)) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            last = Some((i, c));
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                last = Some((i, c));
            }
            '/' if chars.peek().map(|&(_, n)| n) == Some('/') => {
                // Line comment: skip to the newline, which is not significant.
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().map(|&(_, n)| n) == Some('*') => {
                chars.next();
                let mut prev = '\0';
                for (_, c) in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            c if c.is_whitespace() => {}
            c => last = Some((i, c)),
        }
    }
    last
}

/// Remove our block, leaving the rest of the file untouched.
pub fn remove_menu_block(existing: &str) -> String {
    // No block, or one whose end marker is gone: nothing we can safely cut.
    let Block::Found(before, after) = find_block(existing, MENU_BEGIN, MENU_END) else {
        return existing.to_string();
    };
    // The separator we inserted goes with it, or the file accumulates blank
    // lines and stray commas across install/uninstall cycles.
    let before = before.trim_end();
    // Only strip the comma we added, not one that belongs to the user's own
    // trailing entry — which is why this checks it is genuinely the last
    // significant character rather than just the last character.
    let before = match last_significant_char(before) {
        Some(',') => before.strip_suffix(',').unwrap_or(before),
        _ => before,
    };
    format!("{before}\n{}", after.trim_start_matches('\n'))
}

// ── Hyprland layer rule ─────────────────────────────────────────────────────

/// Fences around the block we own inside the user's Hyprland config.
const HYPR_BEGIN: &str = "-- >>> omarchy-dock — managed block, edits are overwritten >>>";
const HYPR_END: &str = "-- <<< omarchy-dock <<<";

fn looknfeel_path() -> PathBuf {
    config_home().join("hypr/looknfeel.lua")
}

/// The blur setup the glass style needs.
///
/// Omarchy ships `decoration.blur.enabled = false`, and Hyprland's per-layer
/// blur does nothing while the subsystem is off — so a layer rule alone is not
/// enough, global blur has to be turned on too. That is a real change to how
/// the whole desktop renders, which is why this is opt-in rather than part of
/// a plain install.
fn hypr_block() -> String {
    format!(
        "{HYPR_BEGIN}\n\
         -- Only needed for `theme.style = \"glass\"`. The Omarchy style is opaque\n\
         -- and needs none of this. Remove with `omarchy-dockctl uninstall`.\n\
         --\n\
         -- Omarchy ships blur disabled globally, and Hyprland's per-layer blur\n\
         -- does nothing until the subsystem is on — so this turns it on, then\n\
         -- opts only the dock's own layer into it.\n\
         hl.config({{ decoration = {{ blur = {{ enabled = true, size = 6, passes = 3 }} }} }})\n\
         hl.layer_rule({{ match = {{ namespace = \"^omarchy-dock$\" }}, blur = true, ignore_alpha = 0.2 }})\n\
         {HYPR_END}"
    )
}

/// Whether blur is already set up for the dock, however it got there.
///
/// Checked by asking Hyprland rather than reading the config: the user may
/// have put it in another file, or written it differently, and offering to add
/// a duplicate would be worse than saying nothing.
fn blur_is_enabled() -> bool {
    std::process::Command::new("hyprctl")
        .args(["getoption", "decoration:blur:enabled", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        // `getoption` types its answer: a bool option reports "bool", an int
        // option reports "int". Reading the wrong one silently yields false.
        .and_then(|v| {
            v.get("bool")
                .and_then(|b| b.as_bool())
                .or_else(|| v.get("int").and_then(|n| n.as_i64()).map(|n| n == 1))
        })
        .unwrap_or(false)
}

fn install_blur(path: PathBuf) -> Result<Report> {
    let original = match read_existing(&path) {
        Ok(text) => text,
        Err(e) => {
            return Ok(Report {
                label: "hyprland blur",
                installed: false,
                note: Some(unreadable_note(&e)),
                path,
            })
        }
    };
    let existing = original.as_deref().unwrap_or_default();

    let next = match find_block(existing, HYPR_BEGIN, HYPR_END) {
        // Replace in place, so an upgrade picks up a changed rule.
        Block::Found(before, after) => format!("{before}{}{after}", hypr_block()),
        Block::Absent => format!("{}\n\n{}\n", existing.trim_end(), hypr_block()),
        Block::Unterminated => {
            return Ok(Report {
                label: "hyprland blur",
                path,
                installed: false,
                note: Some(unterminated_note()),
            })
        }
    };
    let errors_before = hyprland_errors();
    write_file(&path, &next)?;

    // Never leave the user's Hyprland config broken: reload, and if that
    // brings errors that were not there before, put the file back.
    reload_hyprland();
    let errors: Vec<String> =
        hyprland_errors().into_iter().filter(|e| !errors_before.contains(e)).collect();
    if errors.is_empty() {
        return Ok(Report {
            label: "hyprland blur",
            path,
            installed: true,
            note: Some("global blur on, dock layer opted in — needed only by `theme.style = \"glass\"`".into()),
        });
    }
    let note = if restore(&path, &next, original.as_deref())? {
        reload_hyprland();
        format!("Hyprland rejected it, so the file was put back as it was: {}", errors.join("; "))
    } else {
        format!(
            "Hyprland reports errors after this, and the file has changed since, so it was \
             not put back: {}",
            errors.join("; ")
        )
    };
    Ok(Report { label: "hyprland blur", path, installed: false, note: Some(note) })
}

/// Undo a write: put `original` back at `path` (or remove the file, if there
/// was none) — but only while it still holds exactly `written`. If anything
/// has changed it since, that change is not ours to throw away, and `false`
/// says it was left.
fn restore(path: &Path, written: &str, original: Option<&str>) -> Result<bool> {
    if read_existing(path)?.as_deref() != Some(written) {
        return Ok(false);
    }
    match original {
        Some(text) => write_file(path, text)?,
        None => {
            remove_path(path)?;
        }
    }
    Ok(true)
}

fn reload_hyprland() {
    let _ = std::process::Command::new("hyprctl").arg("reload").output();
}

/// The config errors Hyprland currently reports, one per line. Empty when
/// there are none, or when Hyprland is not there to ask.
fn hyprland_errors() -> Vec<String> {
    std::process::Command::new("hyprctl")
        .arg("configerrors")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && *l != "no errors")
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn remove_blur(path: PathBuf) -> Result<Report> {
    let existing = match read_existing(&path) {
        Ok(text) => text.unwrap_or_default(),
        Err(e) => {
            return Ok(Report {
                label: "hyprland blur",
                installed: true,
                note: Some(unreadable_note(&e)),
                path,
            })
        }
    };
    match find_block(&existing, HYPR_BEGIN, HYPR_END) {
        Block::Found(before, after) => {
            let next = format!("{}\n{}", before.trim_end(), after.trim_start_matches('\n'));
            write_file(&path, &next)?;
        }
        Block::Unterminated => {
            return Ok(Report {
                label: "hyprland blur",
                path,
                installed: true,
                note: Some(unterminated_note()),
            })
        }
        Block::Absent => {}
    }
    Ok(Report { label: "hyprland blur", path, installed: false, note: None })
}

/// Add the dock's rows to the menu extension, creating the file if there is
/// none. A file that exists but cannot be read is left as it is.
fn install_menu(path: PathBuf) -> Result<Report> {
    let existing = match read_existing(&path) {
        Ok(text) => text.unwrap_or_else(|| "{\n}\n".into()),
        Err(e) => {
            return Ok(Report {
                label: "menu extension",
                installed: false,
                note: Some(unreadable_note(&e)),
                path,
            })
        }
    };
    Ok(match splice_menu_block(&existing, &menu_block()) {
        Some(next) => {
            write_file(&path, &next)?;
            Report {
                label: "menu extension",
                path,
                installed: true,
                note: Some("`Dock` is now on the Omarchy menu and in its search".into()),
            }
        }
        None => Report {
            label: "menu extension",
            path,
            installed: false,
            note: Some(if matches!(find_block(&existing, MENU_BEGIN, MENU_END), Block::Unterminated) {
                unterminated_note()
            } else {
                "could not find where to insert; leave it and add the rows by hand".into()
            }),
        },
    })
}

/// Take the dock's rows back out of the menu extension, leaving the rest.
fn remove_menu(path: PathBuf) -> Result<Report> {
    let existing = match read_existing(&path) {
        Ok(text) => text.unwrap_or_default(),
        Err(e) => {
            return Ok(Report {
                label: "menu extension",
                installed: true,
                note: Some(unreadable_note(&e)),
                path,
            })
        }
    };
    let next = remove_menu_block(&existing);
    if next != existing {
        write_file(&path, &next)?;
    }
    let kept = next.contains(MENU_BEGIN);
    Ok(Report {
        label: "menu extension",
        installed: kept,
        note: kept.then(unterminated_note),
        path,
    })
}

/// Why a file with a start marker but no end marker was not touched.
fn unterminated_note() -> String {
    "left alone: the dock's start marker is there but its end marker is not, so where \
     its block ends is unknown. Remove the block by hand"
        .into()
}

// ── dock-app keybindings ────────────────────────────────────────────────────

/// Fences around the block we own inside the user's `bindings.lua`.
const KEYS_BEGIN: &str = "-- >>> omarchy-dock keys — managed block, edits are overwritten >>>";
const KEYS_END: &str = "-- <<< omarchy-dock keys <<<";

/// The default chord for "open dock app N".
///
/// Every simpler chord with the number row is already Omarchy's: SUPER,
/// SUPER+SHIFT and SUPER+SHIFT+ALT move between workspaces, SUPER+ALT
/// switches group windows and SUPER+CTRL opens bar panels.
pub const DEFAULT_KEY_MODS: &str = "SUPER + CTRL + ALT";

const MODIFIERS: [&str; 4] = ["SUPER", "SHIFT", "CTRL", "ALT"];

fn bindings_path() -> PathBuf {
    config_home().join("hypr/bindings.lua")
}

/// Canonical form of a modifier chord: "super+alt+ctrl" → "SUPER + CTRL + ALT".
///
/// Only the four real modifiers are accepted. The chord is written into a Lua
/// string literal, so anything else is refused rather than escaped.
pub fn normalize_mods(input: &str) -> Result<String> {
    let mut seen = Vec::new();
    for part in input.split('+').map(|p| p.trim().to_uppercase()) {
        if part.is_empty() {
            continue;
        }
        let part = if part == "CONTROL" { "CTRL".to_string() } else { part };
        anyhow::ensure!(
            MODIFIERS.contains(&part.as_str()),
            "`{part}` is not a modifier; use SUPER, SHIFT, CTRL and ALT"
        );
        if !seen.contains(&part) {
            seen.push(part);
        }
    }
    anyhow::ensure!(!seen.is_empty(), "no modifiers given");
    let ordered: Vec<&str> =
        MODIFIERS.iter().copied().filter(|m| seen.iter().any(|s| s == m)).collect();
    Ok(ordered.join(" + "))
}

fn mod_set(chord: &str) -> Vec<String> {
    let mut v: Vec<String> =
        chord.split('+').map(|p| p.trim().to_uppercase()).filter(|p| !p.is_empty()).collect();
    v.sort();
    v
}

/// Modifier chords that `src` binds together with a number-row key.
///
/// Omarchy builds those bindings in loops — `"SUPER + ALT + code:" ..
/// tostring(i + 9)` or `"SUPER + " .. key` — so the chord is the prefix of a
/// string literal rather than a whole key, and the call often spans several
/// lines. Reading string literals finds all three forms, a literal digit
/// included.
fn number_row_chords(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let lit = &after[..close];
        let tail = after[close + 1..].trim_start();
        let prefix = if let Some(p) = lit.strip_suffix(" + code:") {
            Some(p)
        } else if let Some(p) = lit.strip_suffix(" + ").filter(|_| tail.starts_with("..")) {
            Some(p)
        } else if let Some((p, key)) = lit.rsplit_once(" + ") {
            let digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit() && key != "0";
            let code = key
                .strip_prefix("code:")
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| (10..=18).contains(&n));
            (digit || code).then_some(p)
        } else {
            None
        };
        if let Some(p) = prefix {
            if p.split('+').all(|m| MODIFIERS.contains(&m.trim())) {
                out.push(p.to_string());
            }
        }
        rest = &after[close + 1..];
    }
    out
}

/// Remove our own block from a file's text, so it is not reported as a
/// conflict with itself on reinstall.
fn without_keys_block(src: &str) -> String {
    match find_block(src, KEYS_BEGIN, KEYS_END) {
        Block::Found(before, after) => format!("{before}{after}"),
        Block::Absent | Block::Unterminated => src.to_string(),
    }
}

fn lua_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            lua_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "lua") {
            out.push(p);
        }
    }
}

/// Files that already bind `mods` + a number-row key.
fn keys_conflicts(mods: &str) -> Vec<PathBuf> {
    let want = mod_set(mods);
    let mut files = Vec::new();
    lua_files(Path::new("/usr/share/omarchy/default/hypr"), &mut files);
    if let Some(hypr) = bindings_path().parent() {
        lua_files(hypr, &mut files);
    }
    files
        .into_iter()
        .filter(|f| {
            std::fs::read_to_string(f)
                .map(|src| {
                    number_row_chords(&without_keys_block(&src))
                        .iter()
                        .any(|c| mod_set(c) == want)
                })
                .unwrap_or(false)
        })
        .collect()
}

fn keys_block(mods: &str) -> String {
    format!(
        "{KEYS_BEGIN}\n\
         -- {mods} + 1…9 focuses, cycles or launches the Nth app on the dock.\n\
         -- Installed by `omarchy-dockctl install --keys`; removed by `uninstall`.\n\
         for i = 1, 9 do\n\
         \x20 o.bind(\"{mods} + code:\" .. tostring(i + 9), \"Dock app \" .. i, \"omarchy-dockctl activate \" .. i)\n\
         end\n\
         {KEYS_END}"
    )
}

fn install_keys(mods: &str) -> Result<Report> {
    let mods = normalize_mods(mods)?;
    let conflicts = keys_conflicts(&mods);
    anyhow::ensure!(
        conflicts.is_empty(),
        "{mods} + 1…9 is already bound in {}. Pick another chord, e.g. --keys=\"{DEFAULT_KEY_MODS}\"",
        conflicts.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    );

    let path = bindings_path();
    let original = read_existing(&path)?;
    let before = original.as_deref().unwrap_or_default();
    let next = match find_block(before, KEYS_BEGIN, KEYS_END) {
        Block::Found(head, tail) => format!("{head}{}{tail}", keys_block(&mods)),
        Block::Absent => format!("{}\n\n{}\n", before.trim_end(), keys_block(&mods)),
        Block::Unterminated => anyhow::bail!("{}: {}", path.display(), unterminated_note()),
    };
    let errors_before = hyprland_errors();
    write_file(&path, &next)?;

    // Never leave the user's keybindings broken: if Hyprland objects, put the
    // file back exactly as it was — unless it has changed since.
    reload_hyprland();
    let errors: Vec<String> =
        hyprland_errors().into_iter().filter(|e| !errors_before.contains(e)).collect();
    if !errors.is_empty() {
        let restored = restore(&path, &next, original.as_deref())?;
        reload_hyprland();
        anyhow::ensure!(
            !restored,
            "Hyprland rejected the bindings, so bindings.lua was put back as it was: {}",
            errors.join("; ")
        );
        anyhow::bail!(
            "Hyprland reports errors after adding the bindings, and bindings.lua has changed \
             since, so it was not put back: {}",
            errors.join("; ")
        );
    }
    Ok(Report {
        label: "dock-app keys",
        path,
        installed: true,
        note: Some(format!("{mods} + 1…9 now open dock apps 1–9")),
    })
}

fn remove_keys() -> Result<Report> {
    let path = bindings_path();
    let src = match read_existing(&path) {
        Ok(text) => text.unwrap_or_default(),
        Err(e) => {
            return Ok(Report {
                label: "dock-app keys",
                installed: true,
                note: Some(unreadable_note(&e)),
                path,
            })
        }
    };
    match find_block(&src, KEYS_BEGIN, KEYS_END) {
        Block::Found(head, tail) => {
            write_file(&path, &format!("{}\n{}", head.trim_end(), tail.trim_start_matches('\n')))?;
            reload_hyprland();
        }
        Block::Unterminated => {
            return Ok(Report {
                label: "dock-app keys",
                path,
                installed: true,
                note: Some(unterminated_note()),
            })
        }
        Block::Absent => {}
    }
    Ok(Report { label: "dock-app keys", path, installed: false, note: None })
}

fn keys_installed() -> bool {
    std::fs::read_to_string(bindings_path()).is_ok_and(|s| s.contains(KEYS_BEGIN))
}

// ── install / uninstall ─────────────────────────────────────────────────────

/// Refuse to run where the dock's files would land somewhere they should not.
///
/// Everything here lives in the user's home. Run through `sudo`, it would be
/// written as root: into root's home, or — with the user's `HOME` kept —
/// root-owned into theirs, where they could no longer edit or remove it.
fn preflight() -> Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    anyhow::ensure!(
        unsafe { libc::geteuid() } != 0,
        "run this as your own user, not as root: everything it installs is in your home directory"
    );
    anyhow::ensure!(
        dirs::config_dir().is_some() && dirs::state_dir().is_some(),
        "cannot find your config and state directories (is HOME set?)"
    );
    // The copies are the one thing deleted by name alone, since the dock
    // wrote them. That holds only while their directory is the dock's own and
    // not a link to somewhere else.
    let copies = written_copies_dir();
    for dir in [copies.parent(), Some(copies.as_path())].into_iter().flatten() {
        anyhow::ensure!(
            !is_symlink(dir),
            "{} is a symlink. The dock keeps copies of what it wrote there and deletes \
             them on uninstall, so it has to be the dock's own directory",
            dir.display()
        );
    }
    Ok(())
}

/// Whether anything at all is at `path`. A dangling symlink counts, and so
/// does an entry that cannot be inspected: neither is free to write over.
fn present(path: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// What `install` or `uninstall` did, and the error that stopped it, if one
/// did. Steps run in order and stop at the first failure — a later step may
/// depend on an earlier one — but whatever was already done is still
/// reported, so the report always matches what is on disk.
pub struct Outcome {
    pub reports: Vec<Report>,
    pub error: Option<anyhow::Error>,
}

fn outcome(run: impl FnOnce(&mut Vec<Report>) -> Result<()>) -> Outcome {
    let mut reports = Vec::new();
    let error = run(&mut reports).err();
    Outcome { reports, error }
}

pub fn install(blur: bool, keys: Option<&str>) -> Outcome {
    outcome(|out| install_into(out, blur, keys))
}

pub fn uninstall() -> Outcome {
    outcome(uninstall_into)
}

fn install_into(out: &mut Vec<Report>, blur: bool, keys: Option<&str>) -> Result<()> {
    preflight()?;

    // Hook. Written only over the dock's own: a hook of the same name that
    // someone wrote or edited is left as it is.
    let path = hook_path();
    let copies = written_copies_dir();
    let script = hook_script();
    let hook = DockFile { name: HOOK_NAME, current: Some(&script), released: &[] };
    if present(&path) && !is_dock_file(&path, &hook, Some(&copies)) {
        out.push(Report {
            label: "theme-set hook",
            path,
            installed: false,
            note: Some(
                "left alone: a hook by that name is there that the dock did not write. \
                 Move it aside and run install again"
                    .into(),
            ),
        });
    } else {
        write_executable(&path, &script)
            .with_context(|| format!("installing the theme-set hook at {}", path.display()))?;
        write_file(&copies.join(HOOK_NAME), &script)?;
        out.push(Report {
            label: "theme-set hook",
            path,
            installed: true,
            note: Some("theme changes now reach the dock the moment Omarchy finishes applying them".into()),
        });
    }

    // Plugin.
    //
    // A git checkout is left alone: `omarchy plugin add` clones this
    // repository into exactly this directory, and writing our own copies over
    // its tracked files would leave the checkout dirty and break `omarchy
    // plugin update`. Omarchy itself uses the presence of `.git` to tell a
    // cloned plugin from a hand-written one, so the same test is used here.
    // Anything else is written only if the directory holds nothing but the
    // dock's own files, so an edit or another plugin is never overwritten.
    let legacy = retire_legacy_plugin()?;
    let dir = plugin_dir();
    let git_managed = dir.join(".git").exists();
    let refused = if git_managed {
        if checkout_is_ours(&dir) { Vec::new() } else { vec!["manifest.json".into()] }
    } else {
        write_plugin_files(&dir, &copies)?
    };
    if refused.is_empty() {
        // Enabling happens either way. A clone made by `omarchy plugin add`
        // without `--enable` is present but inert, and skipping this because
        // the files were already there would leave it that way with nothing
        // saying so.
        let enabled = set_plugin_enabled(true)?;
        out.push(Report {
            label: "shell plugin",
            path: dir,
            installed: true,
            note: Some(match (git_managed, enabled) {
                (true, true) => "git checkout left alone; enabled in shell.json".into(),
                (true, false) => "git checkout left alone; already enabled".into(),
                (false, true) => {
                    "enabled in shell.json; the shell now starts and stops the dock".into()
                }
                (false, false) => "already enabled in shell.json".into(),
            }),
        });
    } else {
        out.push(Report {
            label: "shell plugin",
            path: dir,
            installed: false,
            note: Some(format!(
                "left alone, and not enabled: {} {} not what the dock wrote (edited, or \
                 another plugin's). Move {} aside and run install again",
                refused.join(", "),
                if refused.len() == 1 { "is" } else { "are" },
                if refused.len() == 1 { "it" } else { "them" },
            )),
        });
    }
    match legacy {
        Legacy::Absent => {}
        Legacy::Retired => out.push(Report {
            label: "old plugin id",
            path: legacy_plugin_dir(),
            installed: false,
            note: Some(format!("retired `{LEGACY_PLUGIN_ID}`; the plugin is now `{PLUGIN_ID}`")),
        }),
        Legacy::Foreign(left) => out.push(Report {
            label: "old plugin id",
            path: legacy_plugin_dir(),
            installed: false,
            note: Some(format!(
                "left alone: {} {} not what the dock wrote, so `{LEGACY_PLUGIN_ID}` may be \
                 another plugin. If it is only the old dock, run `omarchy plugin remove \
                 {LEGACY_PLUGIN_ID}`",
                left.join(", "),
                if left.len() == 1 { "is" } else { "are" },
            )),
        }),
    }

    out.push(install_menu(menu_extension_path())?);

    // Keybindings are opt-in too: they go into the user's own bindings.lua.
    match keys {
        Some(mods) => out.push(install_keys(mods)?),
        None if !keys_installed() => out.push(Report {
            label: "dock-app keys",
            path: bindings_path(),
            installed: false,
            note: Some(format!(
                "not set up; add {DEFAULT_KEY_MODS} + 1…9 with `omarchy-dockctl install --keys`"
            )),
        }),
        None => {}
    }

    // Blur is opt-in: it turns the effect on for the *whole* desktop, which
    // Omarchy deliberately ships off, and only the glass style needs it.
    if blur {
        out.push(install_blur(looknfeel_path())?);
    } else if !blur_is_enabled() {
        out.push(Report {
            label: "hyprland blur",
            path: looknfeel_path(),
            installed: false,
            note: Some(
                "not set up. Only `theme.style = \"glass\"` needs it; add it with \
                 `omarchy-dockctl install --blur`"
                    .into(),
            ),
        });
    }

    Ok(())
}

fn uninstall_into(out: &mut Vec<Report>) -> Result<()> {
    preflight()?;

    // Only the dock's own hook: one someone edited, or replaced with their
    // own under the same name, stays.
    let path = hook_path();
    let copies = written_copies_dir();
    let script = hook_script();
    let hook = DockFile { name: HOOK_NAME, current: Some(&script), released: &[] };
    let ours = is_dock_file(&path, &hook, Some(&copies));
    if ours {
        remove_path(&path)?;
    }
    remove_path(&copies.join(HOOK_NAME))?;
    let kept = present(&path);
    out.push(Report {
        label: "theme-set hook",
        installed: kept,
        note: kept.then(|| "left in place: it is not the hook the dock wrote".into()),
        path,
    });

    // Give the bar back its workspaces if the dock took them out. Only a
    // widget the dock removed is restored; one the user removed stays off.
    let note = crate::bar_widgets::sync(crate::bar_widgets::WORKSPACES, false)
        .err()
        .map(|e| format!("could not put the bar's workspaces back: {e}"));
    out.push(Report { label: "bar workspaces", path: shell_json_path(), installed: false, note });

    // Drop the shell.json reference before the files, so the shell is never
    // pointed at a plugin directory that has just been deleted.
    set_plugin_enabled(false)?;
    retire_legacy_plugin()?;
    let dir = plugin_dir();
    let git_managed = dir.join(".git").exists();
    // Deleting someone's git checkout is not ours to do; disabling it in
    // shell.json already stops the dock, and `omarchy plugin remove` is the
    // command that owns removing it.
    let left = if git_managed {
        Vec::new()
    } else {
        remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies))?
    };
    out.push(Report {
        label: "shell plugin",
        installed: dir.exists(),
        note: if git_managed {
            Some(format!("disabled; remove the checkout with `omarchy plugin remove {PLUGIN_ID}`"))
        } else if !left.is_empty() {
            Some(format!(
                "disabled; left {} in place, which the dock did not write",
                left.join(", ")
            ))
        } else {
            None
        },
        path: dir,
    });

    out.push(remove_menu(menu_extension_path())?);
    out.push(remove_blur(looknfeel_path())?);
    out.push(remove_keys()?);

    // The dock's own state directories, if that left them empty.
    for dir in copies.ancestors().take(2) {
        let _ = std::fs::remove_dir(dir);
    }

    Ok(())
}

/// What is currently installed, without changing anything.
pub fn status() -> Result<Vec<Report>> {
    anyhow::ensure!(
        dirs::config_dir().is_some() && dirs::state_dir().is_some(),
        "cannot find your config and state directories (is HOME set?)"
    );
    let menu = menu_extension_path();
    let menu_installed = std::fs::read_to_string(&menu)
        .map(|s| s.contains(MENU_BEGIN))
        .unwrap_or(false);

    Ok(vec![
        Report {
            label: "theme-set hook",
            installed: present(&hook_path()),
            path: hook_path(),
            note: None,
        },
        Report {
            label: "shell plugin",
            installed: plugin_dir().join("manifest.json").exists(),
            path: plugin_dir(),
            note: (!plugin_is_enabled())
                .then(|| "installed but not enabled in shell.json".into()),
        },
        Report { label: "menu extension", installed: menu_installed, path: menu, note: None },
        Report {
            label: "dock-app keys",
            installed: keys_installed(),
            path: bindings_path(),
            note: None,
        },
        Report {
            label: "hyprland blur",
            installed: blur_is_enabled(),
            path: looknfeel_path(),
            note: Some("only `theme.style = \"glass\"` needs it".into()),
        },
    ])
}

// ── file helpers ────────────────────────────────────────────────────────────

/// A file's text, or `None` if it does not exist yet. Every other failure —
/// no permission, contents that are not UTF-8, an I/O error — is an error and
/// never an empty file: an edit made to "nothing" and written back would
/// replace whatever the file really holds.
fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Why a file that could not be read was not touched.
fn unreadable_note(e: &anyhow::Error) -> String {
    format!("left alone: {e:#}")
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    crate::safe_write::replace(path, contents.as_bytes())
}

fn write_executable(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    write_file(path, contents)?;
    let mut perms = std::fs::metadata(path)?.permissions();
    // Omarchy only runs hooks that are executable.
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("making {} executable", path.display()))
}

fn remove_path(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn modifier_chords_are_normalised_and_validated() {
        assert_eq!(normalize_mods("super+alt+ctrl").unwrap(), "SUPER + CTRL + ALT");
        assert_eq!(normalize_mods(" SUPER + Control ").unwrap(), "SUPER + CTRL");
        // Anything that is not a modifier is refused: it lands in Lua source.
        assert!(normalize_mods("SUPER + \"); os.execute(\"x").is_err());
        assert!(normalize_mods("").is_err());
    }

    #[test]
    fn omarchys_number_row_loops_are_recognised() {
        // The three shapes Omarchy's own bindings use, including a call split
        // over several lines.
        let src = r#"
  o.bind("SUPER + " .. key, "Switch to workspace " .. workspace, x)
  o.bind("SUPER + ALT + code:" .. tostring(index + 9), "Switch to group window " .. index, y)
  o.bind(
    "SUPER + CTRL + code:" .. tostring(panel + 9),
    "Bar panel " .. panel,
  o.bind("SUPER + SHIFT + R", "SSH", "alacritty")
  o.bind("CTRL + ALT + 3", "three", "z")
"#;
        let mut chords = number_row_chords(src);
        chords.sort();
        assert_eq!(chords, ["CTRL + ALT", "SUPER", "SUPER + ALT", "SUPER + CTRL"]);
        // SUPER + SHIFT + R is a letter, not the number row.
    }

    #[test]
    fn chords_compare_regardless_of_order() {
        assert_eq!(mod_set("ALT + SUPER + CTRL"), mod_set("SUPER + CTRL + ALT"));
        assert_ne!(mod_set("SUPER + ALT"), mod_set("SUPER + CTRL + ALT"));
    }

    #[test]
    fn our_own_block_is_not_a_conflict_with_itself() {
        let src = format!("-- mine\n{}\n", keys_block(DEFAULT_KEY_MODS));
        assert_eq!(number_row_chords(&src), [DEFAULT_KEY_MODS]);
        assert!(number_row_chords(&without_keys_block(&src)).is_empty());
    }

    use super::*;

    #[test]
    fn the_manifest_version_matches_the_crate() {
        // The manifest is a checked-in file rather than generated, so nothing
        // stops it drifting from Cargo.toml — except this.
        let manifest: serde_json::Value =
            serde_json::from_str(PLUGIN_MANIFEST).expect("manifest.json is valid JSON");
        assert_eq!(
            manifest["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "manifest.json version must match Cargo.toml"
        );
        assert_eq!(manifest["id"].as_str(), Some(PLUGIN_ID));
        // The entry point has to name a file that is actually in the repo, or
        // `omarchy plugin add` clones something the shell cannot load.
        assert_eq!(manifest["entryPoints"]["service"].as_str(), Some("Service.qml"));
    }

    /// A fresh plugin directory and copies directory under the temp dir.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir()
            .join(format!("omarchy-dock-integrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (root.join("plugin"), root.join("copies"))
    }

    #[test]
    fn shell_json_keeps_its_key_order() {
        // shell.json is the user's file; enabling the plugin must add one
        // entry, not re-sort every object in it.
        let text = r#"{"bar":{"layout":{"center":[{"id":"omarchy.clock","format":"HH:mm"}]}},"plugins":[]}"#;
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&json).unwrap(), text);
    }

    #[test]
    fn the_dock_removes_its_own_plugin_directory() {
        let (dir, copies) = scratch("own");
        write_plugin_files(&dir, &copies).unwrap();
        assert!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap().is_empty());
        assert!(!dir.exists());
        assert!(!copies.join("Service.qml").exists());
    }

    #[test]
    fn files_the_dock_did_not_write_are_left_with_their_directory() {
        let (dir, copies) = scratch("foreign");
        write_plugin_files(&dir, &copies).unwrap();
        write_file(&dir.join("notes.txt"), "mine").unwrap();
        assert_eq!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap(), ["notes.txt"]);
        assert!(dir.join("notes.txt").exists());
        assert!(!dir.join("manifest.json").exists());
    }

    #[test]
    fn an_edited_plugin_file_is_the_users() {
        let (dir, copies) = scratch("edited");
        write_plugin_files(&dir, &copies).unwrap();
        write_file(&dir.join("Service.qml"), "// changed by hand\n").unwrap();
        assert_eq!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap(), ["Service.qml"]);
        assert_eq!(std::fs::read_to_string(dir.join("Service.qml")).unwrap(), "// changed by hand\n");
    }

    #[test]
    fn a_file_an_earlier_build_wrote_is_recognised_by_its_copy() {
        let (dir, copies) = scratch("earlier");
        write_file(&dir.join("Service.qml"), "// an older Service.qml\n").unwrap();
        write_file(&copies.join("Service.qml"), "// an older Service.qml\n").unwrap();
        assert!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap().is_empty());
        assert!(!dir.exists());
    }

    #[test]
    fn install_never_overwrites_an_edited_file() {
        let (dir, copies) = scratch("install-edited");
        write_plugin_files(&dir, &copies).unwrap();
        write_file(&dir.join("Service.qml"), "// changed by hand\n").unwrap();
        assert_eq!(write_plugin_files(&dir, &copies).unwrap(), ["Service.qml"]);
        assert_eq!(std::fs::read_to_string(dir.join("Service.qml")).unwrap(), "// changed by hand\n");
    }

    #[test]
    fn install_never_writes_into_another_plugin() {
        let (dir, copies) = scratch("install-other");
        write_file(&dir.join("main.qml"), "// another plugin\n").unwrap();
        assert_eq!(write_plugin_files(&dir, &copies).unwrap(), ["main.qml"]);
        assert!(!dir.join("manifest.json").exists());

        let (dir, copies) = scratch("install-other-manifest");
        write_file(&dir.join("manifest.json"), "{\"id\":\"someone.else\"}").unwrap();
        assert_eq!(write_plugin_files(&dir, &copies).unwrap(), ["manifest.json"]);
        assert_eq!(std::fs::read_to_string(dir.join("manifest.json")).unwrap(), "{\"id\":\"someone.else\"}");
    }

    #[test]
    fn install_upgrades_what_a_release_wrote() {
        // Even with no copies kept, every release's files are known.
        for release in 0..PLUGIN_FILES[0].released.len() {
            let (dir, copies) = scratch(&format!("install-upgrade-{release}"));
            for file in &PLUGIN_FILES {
                write_file(&dir.join(file.name), file.released[release]).unwrap();
            }
            assert!(write_plugin_files(&dir, &copies).unwrap().is_empty());
            assert_eq!(std::fs::read_to_string(dir.join("Service.qml")).unwrap(), PLUGIN_SERVICE_QML);
        }
    }

    #[test]
    fn an_empty_or_missing_directory_is_installed_into() {
        let (dir, copies) = scratch("install-fresh");
        assert!(write_plugin_files(&dir, &copies).unwrap().is_empty());
        assert!(dir.join("manifest.json").exists() && copies.join("manifest.json").exists());
    }

    #[test]
    fn the_old_plugin_goes_only_if_it_is_what_1_2_0_wrote() {
        let (dir, _) = scratch("legacy-ours");
        for file in &LEGACY_PLUGIN_FILES {
            write_file(&dir.join(file.name), file.released[0]).unwrap();
        }
        assert!(remove_plugin_files(&dir, &LEGACY_PLUGIN_FILES, None).unwrap().is_empty());
        assert!(!dir.exists());

        let (dir, _) = scratch("legacy-other");
        write_file(&dir.join("manifest.json"), "{\"id\":\"omarchy-dock\",\"name\":\"not ours\"}").unwrap();
        assert_eq!(remove_plugin_files(&dir, &LEGACY_PLUGIN_FILES, None).unwrap(), ["manifest.json"]);
    }

    #[test]
    fn the_released_files_are_the_releases() {
        // Each release's files name the id that release used.
        let id = |text: &str| {
            serde_json::from_str::<serde_json::Value>(text).unwrap()["id"].as_str().unwrap().to_string()
        };
        for released in PLUGIN_FILES[0].released {
            assert_eq!(id(released), PLUGIN_ID);
        }
        assert_eq!(id(LEGACY_PLUGIN_FILES[0].released[0]), LEGACY_PLUGIN_ID);
    }

    #[test]
    fn someone_elses_plugin_is_untouched() {
        // No copies: nothing the dock wrote, so nothing the dock may delete.
        let (dir, copies) = scratch("other");
        write_file(&dir.join("manifest.json"), "{\"id\":\"omarchy-dock\"}").unwrap();
        write_file(&dir.join("Service.qml"), "// another plugin\n").unwrap();
        assert_eq!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap(), ["Service.qml", "manifest.json"]);
    }

    #[test]
    fn a_start_marker_without_its_end_leaves_the_file_alone() {
        // Everything after the start marker would otherwise be taken for the
        // dock's block: the user's own rows below it would be lost.
        let existing = format!("{{\n{MENU_BEGIN}\n  \"mine\": {{\"label\":\"Mine\"}},\n}}\n");
        assert_eq!(splice_menu_block(&existing, "BLOCK"), None);
        assert_eq!(remove_menu_block(&existing), existing);
        assert!(matches!(find_block(&existing, MENU_BEGIN, MENU_END), Block::Unterminated));
    }

    #[test]
    fn a_block_is_inserted_before_the_closing_brace() {
        let out = splice_menu_block("{\n}\n", "BLOCK").unwrap();
        assert!(out.contains("BLOCK"));
        // Still a single object, and our block is inside it.
        assert!(out.trim_end().ends_with('}'));
        assert!(out.find("BLOCK").unwrap() < out.rfind('}').unwrap());
    }

    #[test]
    fn an_existing_entry_gets_a_separating_comma() {
        let existing = "{\n  \"a\": {\"label\":\"A\"}\n}\n";
        let out = splice_menu_block(existing, "BLOCK").unwrap();
        // Without the comma the file would no longer parse.
        assert!(out.contains("\"A\"},\nBLOCK"), "{out}");
    }

    #[test]
    fn a_trailing_comma_is_not_doubled() {
        let existing = "{\n  \"a\": {\"label\":\"A\"},\n}\n";
        let out = splice_menu_block(existing, "BLOCK").unwrap();
        assert!(!out.contains(",,"), "{out}");
    }

    #[test]
    fn an_all_comments_file_needs_no_comma() {
        // Omarchy ships the extension file as comments and an empty object.
        let existing = "{\n  // examples\n  // \"x\": {}\n}\n";
        let out = splice_menu_block(existing, "BLOCK").unwrap();
        assert!(!out.contains(",\nBLOCK"), "{out}");
        assert!(out.contains("BLOCK"));
    }

    #[test]
    fn reinstalling_replaces_the_block_rather_than_repeating_it() {
        let first = splice_menu_block("{\n}\n", &wrapped("OLD")).unwrap();
        let second = splice_menu_block(&first, &wrapped("NEW")).unwrap();
        assert!(second.contains("NEW"));
        assert!(!second.contains("OLD"));
        assert_eq!(second.matches(MENU_BEGIN).count(), 1);
    }

    #[test]
    fn uninstalling_restores_what_the_user_had() {
        let original = "{\n  \"a\": {\"label\":\"A\"}\n}\n";
        let installed = splice_menu_block(original, &wrapped("BLOCK")).unwrap();
        assert!(installed.contains("BLOCK"));

        let removed = remove_menu_block(&installed);
        assert!(!removed.contains("BLOCK"));
        assert!(!removed.contains(MENU_BEGIN));
        // The user's own entry survives, and no stray comma is left behind.
        assert!(removed.contains("\"a\""));
        assert!(!removed.contains("},\n\n}"), "{removed}");
    }

    #[test]
    fn removing_from_a_file_we_never_touched_changes_nothing() {
        let original = "{\n  \"a\": {\"label\":\"A\"}\n}\n";
        assert_eq!(remove_menu_block(original), original);
    }

    #[test]
    fn a_file_with_no_object_is_left_alone() {
        // Nothing to insert into means we must not guess.
        assert!(splice_menu_block("", "BLOCK").is_none());
        assert!(splice_menu_block("// only comments\n", "BLOCK").is_none());
    }

    #[test]
    fn the_generated_menu_block_carries_both_markers() {
        let block = menu_block();
        assert!(block.starts_with(MENU_BEGIN));
        assert!(block.ends_with(MENU_END));
        // Every row must be reachable without the dock running, or the menu
        // would offer dead entries after a crash.
        assert!(block.contains("omarchy-dockctl"));
    }

    #[test]
    fn comments_do_not_count_as_content_when_deciding_on_a_comma() {
        assert_eq!(last_significant_char("{\n  // \"x\": {}\n"), Some('{'));
        assert_eq!(last_significant_char("{\n  /* \"x\": {} */\n"), Some('{'));
        // A real entry does count, including one ending inside a string.
        assert_eq!(last_significant_char("{\n  \"a\": {\"label\":\"A\"}\n"), Some('}'));
        assert_eq!(last_significant_char("{\n  \"a\": \"b\"\n"), Some('"'));
        // A `//` inside a string is not a comment.
        assert_eq!(last_significant_char("{\n  \"a\": \"http://x\"\n"), Some('"'));
    }

    #[test]
    fn a_url_in_a_description_does_not_break_the_comma() {
        let existing = "{\n  \"a\": {\"description\":\"see http://x\"}\n}\n";
        let out = splice_menu_block(existing, "BLOCK").unwrap();
        assert!(out.contains("},\nBLOCK"), "{out}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_replaced() {
        let (dir, _) = scratch("unreadable");
        std::fs::create_dir_all(&dir).unwrap();
        // Not UTF-8, so it cannot be read as text; what is there must survive.
        let bytes: &[u8] = b"{\n  \"mine\": 1 \xff\n}\n";
        let menu = dir.join("omarchy-menu.jsonc");
        let looknfeel = dir.join("looknfeel.conf");
        for path in [&menu, &looknfeel] {
            std::fs::write(path, bytes).unwrap();
        }

        let report = install_menu(menu.clone()).unwrap();
        assert!(!report.installed);
        assert!(report.note.unwrap().starts_with("left alone"));
        let report = install_blur(looknfeel.clone()).unwrap();
        assert!(!report.installed);
        assert!(remove_menu(menu.clone()).unwrap().installed);
        assert!(remove_blur(looknfeel.clone()).unwrap().installed);

        for path in [&menu, &looknfeel] {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn a_missing_menu_file_is_created() {
        let (dir, _) = scratch("menu-new");
        let menu = dir.join("omarchy-menu.jsonc");
        assert!(install_menu(menu.clone()).unwrap().installed);
        let text = std::fs::read_to_string(&menu).unwrap();
        assert!(text.contains(MENU_BEGIN) && text.contains(MENU_END));
        assert!(!remove_menu(menu.clone()).unwrap().installed);
        assert!(!std::fs::read_to_string(&menu).unwrap().contains(MENU_BEGIN));
    }

    #[test]
    fn a_brace_in_a_trailing_comment_is_not_the_closing_one() {
        let existing = "{\n  \"a\": 1\n}\n// e.g. \"b\": {\"x\": {}}\n";
        let out = splice_menu_block(existing, "BLOCK").unwrap();
        assert!(out.starts_with("{\n  \"a\": 1,\nBLOCK\n}"), "{out}");
        assert!(out.ends_with("// e.g. \"b\": {\"x\": {}}\n"), "{out}");
        // No object to close at all: nothing to insert into.
        assert!(splice_menu_block("// just a comment }\n", "BLOCK").is_none());
    }

    #[test]
    fn a_symlinked_plugin_directory_is_left_alone() {
        let (dir, copies) = scratch("symlinked");
        let real = dir.with_file_name("real-plugin");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &dir).unwrap();
        for file in &PLUGIN_FILES {
            std::fs::write(real.join(file.name), file.current.unwrap()).unwrap();
        }
        assert_eq!(write_plugin_files(&dir, &copies).unwrap(), vec!["plugin (a symlink)"]);
        assert_eq!(remove_plugin_files(&dir, &PLUGIN_FILES, Some(&copies)).unwrap(), vec!["plugin (a symlink)"]);
        assert!(is_symlink(&dir) && real.join("manifest.json").exists());
    }

    #[test]
    fn a_dangling_symlink_is_present() {
        let (dir, _) = scratch("dangling");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("hook");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).unwrap();
        assert!(present(&link) && !link.exists());
        assert!(!present(&dir.join("nothing")));
    }

    #[test]
    fn a_rollback_only_undoes_what_is_still_ours() {
        let (dir, _) = scratch("restore");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("looknfeel.lua");
        std::fs::write(&path, "ours").unwrap();
        assert!(restore(&path, "ours", Some("theirs")).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs");
        // Changed since we wrote it: kept.
        assert!(!restore(&path, "ours", Some("older")).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs");
        // There was no file before us: it goes.
        std::fs::write(&path, "ours").unwrap();
        assert!(restore(&path, "ours", None).unwrap());
        assert!(!present(&path));
    }

    fn wrapped(body: &str) -> String {
        format!("{MENU_BEGIN}\n{body}\n{MENU_END}")
    }
}
