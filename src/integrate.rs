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
//! Everything written here is confined to `~/.config/omarchy/`, is marked as
//! belonging to the dock, and is removed cleanly. The menu extension is the
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

pub fn omarchy_config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("omarchy")
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
        .unwrap_or_else(|| PathBuf::from("."))
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
            include_str!("../resources/released/1.2.2/manifest.json.txt"),
            include_str!("../resources/released/1.2.1/manifest.json.txt"),
        ],
    },
    DockFile {
        name: "Service.qml",
        current: Some(PLUGIN_SERVICE_QML),
        released: &[
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
    let Ok(text) = std::fs::read_to_string(&path) else {
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
        std::fs::write(&path, out)
            .with_context(|| format!("writing {}", path.display()))?;
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

    // Insert before the final closing brace of the JSONC object.
    let at = existing.rfind('}')?;
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
    let mut last = None;
    let mut chars = s.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            last = Some(c);
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                last = Some(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                // Line comment: skip to the newline, which is not significant.
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            c if c.is_whitespace() => {}
            c => last = Some(c),
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
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("hypr/looknfeel.lua")
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

fn install_blur() -> Result<Report> {
    let path = looknfeel_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let next = match find_block(&existing, HYPR_BEGIN, HYPR_END) {
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
    write_file(&path, &next)?;

    // Hyprland reloads on save; ask it whether what we wrote actually parses,
    // rather than leaving a broken config behind and saying nothing.
    let errors = std::process::Command::new("hyprctl")
        .arg("configerrors")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let note = if errors.is_empty() || errors == "no errors" {
        "global blur on, dock layer opted in — needed only by `theme.style = \"glass\"`".into()
    } else {
        format!("Hyprland reports config errors after this: {errors}")
    };

    Ok(Report { label: "hyprland blur", path, installed: true, note: Some(note) })
}

fn remove_blur() -> Result<Report> {
    let path = looknfeel_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
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

/// Why a file with a start marker but no end marker was not touched.
fn unterminated_note() -> String {
    "left alone: the dock's start marker is there but its end marker is not, so where \
     its block ends is unknown. Remove the block by hand"
        .into()
}

// ── install / uninstall ─────────────────────────────────────────────────────

pub fn install(blur: bool) -> Result<Vec<Report>> {
    let mut out = Vec::new();

    // Hook. Written only over the dock's own: a hook of the same name that
    // someone wrote or edited is left as it is.
    let path = hook_path();
    let copies = written_copies_dir();
    let script = hook_script();
    let hook = DockFile { name: HOOK_NAME, current: Some(&script), released: &[] };
    if path.exists() && !is_dock_file(&path, &hook, Some(&copies)) {
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

    // Menu extension.
    let path = menu_extension_path();
    let existing = read_or_default_menu(&path);
    match splice_menu_block(&existing, &menu_block()) {
        Some(next) => {
            write_file(&path, &next)?;
            out.push(Report {
                label: "menu extension",
                path,
                installed: true,
                note: Some("`Dock` is now on the Omarchy menu and in its search".into()),
            });
        }
        None => out.push(Report {
            label: "menu extension",
            path,
            installed: false,
            note: Some(if matches!(find_block(&existing, MENU_BEGIN, MENU_END), Block::Unterminated) {
                unterminated_note()
            } else {
                "could not find where to insert; leave it and add the rows by hand".into()
            }),
        }),
    }

    // Blur is opt-in: it turns the effect on for the *whole* desktop, which
    // Omarchy deliberately ships off, and only the glass style needs it.
    if blur {
        out.push(install_blur()?);
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

    Ok(out)
}

pub fn uninstall() -> Result<Vec<Report>> {
    let mut out = Vec::new();

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
    let kept = path.exists();
    out.push(Report {
        label: "theme-set hook",
        installed: kept,
        note: kept.then(|| "left in place: it is not the hook the dock wrote".into()),
        path,
    });

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

    let path = menu_extension_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let next = remove_menu_block(&existing);
    if next != existing {
        write_file(&path, &next)?;
    }
    let kept = next.contains(MENU_BEGIN);
    out.push(Report {
        label: "menu extension",
        installed: kept,
        note: kept.then(unterminated_note),
        path,
    });
    out.push(remove_blur()?);

    Ok(out)
}

/// What is currently installed, without changing anything.
pub fn status() -> Vec<Report> {
    let menu = menu_extension_path();
    let menu_installed = std::fs::read_to_string(&menu)
        .map(|s| s.contains(MENU_BEGIN))
        .unwrap_or(false);

    vec![
        Report {
            label: "theme-set hook",
            installed: hook_path().exists(),
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
            label: "hyprland blur",
            installed: blur_is_enabled(),
            path: looknfeel_path(),
            note: Some("only `theme.style = \"glass\"` needs it".into()),
        },
    ]
}

// ── file helpers ────────────────────────────────────────────────────────────

fn read_or_default_menu(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "{\n}\n".to_string())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("writing {}", path.display()))
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

    fn wrapped(body: &str) -> String {
        format!("{MENU_BEGIN}\n{body}\n{MENU_END}")
    }
}
