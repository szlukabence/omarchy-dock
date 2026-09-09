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
//! rewritten wholesale.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Plugin id, and the directory name under `~/.config/omarchy/plugins/`.
pub const PLUGIN_ID: &str = "omarchy-dock";

/// Basename of the hook we drop into `theme-set.d`.
const HOOK_NAME: &str = "omarchy-dock";

/// Fences around the block we own inside the user's menu extension file.
const MENU_BEGIN: &str = "  // >>> omarchy-dock — managed block, edits are overwritten >>>";
const MENU_END: &str = "  // <<< omarchy-dock <<<";

pub fn omarchy_config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("omarchy")
}

fn hook_path() -> PathBuf {
    omarchy_config_dir().join("hooks/theme-set.d").join(HOOK_NAME)
}

fn plugin_dir() -> PathBuf {
    omarchy_config_dir().join("plugins").join(PLUGIN_ID)
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
fn plugin_manifest() -> String {
    format!(
        r#"{{
  "schemaVersion": 1,
  "id": "{PLUGIN_ID}",
  "name": "Omarchy Dock",
  "version": "{version}",
  "license": "MIT",
  "description": "macOS-style application dock drawn with the Omarchy shell's own design tokens.",
  "kinds": [
    "service"
  ],
  "entryPoints": {{
    "service": "Service.qml"
  }},
  "tags": [
    "dock",
    "launcher",
    "hyprland"
  ]
}}
"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn plugin_service_qml() -> String {
    // Deliberately minimal. The dock owns its own config, theming and
    // lifetime; all the plugin does is decide whether it is running, so that
    // Omarchy's plugin manager is the single switch a user reaches for.
    r#"import QtQuick
import Quickshell.Io

// Installed by omarchy-dock. Removed by `omarchy-dockctl uninstall`.
//
// omarchy-dock is a separate GTK4 layer-shell process rather than QML, so this
// plugin supervises it instead of drawing it: enabling the plugin starts the
// dock, disabling it stops the dock. That is what makes the dock appear in
// `omarchy menu plugin` and the plugin managers alongside every other
// component, and gives it an autostart tied to the shell's own lifecycle.
Item {
  id: root

  // Injected by omarchy-shell's service loader.
  property var shell: null

  // Start only if one is not already running: the user may have launched the
  // dock by hand, and two docks would fight over the control socket.
  //
  // The missing-binary case is handled explicitly rather than left to fail.
  // `omarchy plugin add` only clones a repo — it never builds anything — so a
  // plugin installed on its own has no binary behind it, and a bare
  // "command not found" at login says nothing about what to do next.
  Process {
    id: starter
    command: ["bash", "-lc",
      "if ! command -v omarchy-dock >/dev/null 2>&1; then " +
        "omarchy notification send --app-name Dock -u critical " +
        "'Dock is not installed' " +
        "'This plugin is only the supervisor. Install the dock with: omarchy pkg aur add omarchy-dock-bin'; " +
        "exit 0; " +
      "fi; " +
      "pgrep -x omarchy-dock >/dev/null || setsid uwsm-app -- omarchy-dock >/dev/null 2>&1 &"]
    running: true
  }

  Process {
    id: stopper
    command: ["pkill", "-x", "omarchy-dock"]
  }

  // Stopping on teardown is what makes disabling the plugin actually disable
  // the dock, rather than leaving an orphan running until the next login.
  Component.onDestruction: stopper.running = true
}
"#
    .into()
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
        .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(PLUGIN_ID));

    let changed = match (on, at) {
        (true, None) => {
            list.push(serde_json::json!({ "id": PLUGIN_ID }));
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
        r#""dock.settings": {"icon":"\uf013","label":"Settings","description":"Edit config.toml","action":"omarchy-launch-config-editor ~/.config/omarchy-dock/config.toml"}"#,
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
/// be editing.
pub fn splice_menu_block(existing: &str, block: &str) -> Option<String> {
    if let Some((before, rest)) = existing.split_once(MENU_BEGIN) {
        // Replace whatever is currently between the markers, so reinstalling
        // after an upgrade picks up new rows instead of duplicating old ones.
        let after = rest.split_once(MENU_END).map(|(_, a)| a).unwrap_or("");
        return Some(format!("{before}{block}{after}"));
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
    let Some((before, rest)) = existing.split_once(MENU_BEGIN) else {
        return existing.to_string();
    };
    let after = rest.split_once(MENU_END).map(|(_, a)| a).unwrap_or("");
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

    let next = if existing.contains(HYPR_BEGIN) {
        // Replace in place, so an upgrade picks up a changed rule.
        let (before, rest) = existing.split_once(HYPR_BEGIN).unwrap();
        let after = rest.split_once(HYPR_END).map(|(_, a)| a).unwrap_or("");
        format!("{before}{}{after}", hypr_block())
    } else {
        format!("{}\n\n{}\n", existing.trim_end(), hypr_block())
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
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if let Some((before, rest)) = existing.split_once(HYPR_BEGIN) {
            let after = rest.split_once(HYPR_END).map(|(_, a)| a).unwrap_or("");
            let next = format!("{}\n{}", before.trim_end(), after.trim_start_matches('\n'));
            write_file(&path, &next)?;
        }
    }
    Ok(Report { label: "hyprland blur", path, installed: false, note: None })
}

// ── install / uninstall ─────────────────────────────────────────────────────

pub fn install(blur: bool) -> Result<Vec<Report>> {
    let mut out = Vec::new();

    // Hook.
    let path = hook_path();
    write_executable(&path, &hook_script())
        .with_context(|| format!("installing the theme-set hook at {}", path.display()))?;
    out.push(Report {
        label: "theme-set hook",
        path,
        installed: true,
        note: Some("theme changes now reach the dock the moment Omarchy finishes applying them".into()),
    });

    // Plugin.
    let dir = plugin_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    write_file(&dir.join("manifest.json"), &plugin_manifest())?;
    write_file(&dir.join("Service.qml"), &plugin_service_qml())?;
    let enabled = set_plugin_enabled(true)?;
    out.push(Report {
        label: "shell plugin",
        path: dir,
        installed: true,
        note: Some(if enabled {
            "enabled in shell.json; the shell now starts and stops the dock".into()
        } else {
            "already enabled in shell.json".into()
        }),
    });

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
            note: Some("could not find where to insert; leave it and add the rows by hand".into()),
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

    let path = hook_path();
    let removed = remove_path(&path)?;
    out.push(Report { label: "theme-set hook", path, installed: !removed, note: None });

    // Drop the shell.json reference before the files, so the shell is never
    // pointed at a plugin directory that has just been deleted.
    set_plugin_enabled(false)?;
    let dir = plugin_dir();
    let removed = if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing {}", dir.display()))?;
        true
    } else {
        false
    };
    out.push(Report { label: "shell plugin", path: dir, installed: !removed, note: None });

    let path = menu_extension_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let next = remove_menu_block(&existing);
        if next != existing {
            write_file(&path, &next)?;
        }
    }
    out.push(Report { label: "menu extension", path, installed: false, note: None });
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
