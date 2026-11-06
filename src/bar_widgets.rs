//! Hiding the bar's own workspace widget while the dock shows workspaces.
//!
//! Two rows of the same workspace numbers, one on the bar and one on the dock,
//! is one too many, so the dock can take the bar's out while its strip is on.
//! It goes through the shell's own plugin switch — what `omarchy plugin
//! disable` does — rather than editing the bar layout behind the shell's back.
//!
//! Disabling a bar widget drops it from the layout, and enabling it again
//! needs a placement, so before hiding the widget the dock records where it
//! sat: its section, its neighbours and its index. Putting it back uses the
//! neighbour when it is still there and the index when it is not, so the
//! widget lands where the user had it, not at a default spot.
//!
//! The record doubles as proof of ownership. The dock only ever restores a
//! widget it removed itself: one the user took off the bar stays off.
//!
//! Shared by both binaries, so it depends on nothing else in the crate.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// The bar's workspace widget.
pub const WORKSPACES: &str = "omarchy.workspaces";

const SECTIONS: [&str; 3] = ["left", "center", "right"];

/// Where a bar widget sat before the dock took it out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Placement {
    section: String,
    index: usize,
    after: Option<String>,
    before: Option<String>,
}

fn shell_json_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("omarchy/shell.json"))
}

/// The record of a widget the dock has hidden.
fn record_path(id: &str) -> Option<PathBuf> {
    Some(dirs::state_dir()?.join("omarchy-dock").join(format!("hidden-bar-widget-{id}.json")))
}

fn entry_id(entry: &Value) -> Option<&str> {
    entry.get("id").and_then(Value::as_str).or_else(|| entry.as_str())
}

/// Where widget `id` sits in the bar layout of `shell`, if it is there.
fn locate(shell: &Value, id: &str) -> Option<Placement> {
    let layout = shell.get("bar")?.get("layout")?;
    for section in SECTIONS {
        let Some(list) = layout.get(section).and_then(Value::as_array) else { continue };
        let ids: Vec<Option<&str>> = list.iter().map(entry_id).collect();
        if let Some(index) = ids.iter().position(|e| *e == Some(id)) {
            return Some(Placement {
                section: section.to_string(),
                index,
                after: index.checked_sub(1).and_then(|i| ids[i]).map(str::to_string),
                before: ids.get(index + 1).copied().flatten().map(str::to_string),
            });
        }
    }
    None
}

/// The placement to hand `enablePlugin` so the widget returns to where
/// `saved` says it was, given the bar as it is now.
fn restore_placement(shell: &Value, saved: &Placement) -> Value {
    let present = |id: &Option<String>| {
        id.as_deref().is_some_and(|id| {
            locate(shell, id).is_some_and(|p| p.section == saved.section)
        })
    };
    if present(&saved.after) {
        serde_json::json!({ "section": saved.section, "after": saved.after })
    } else if present(&saved.before) {
        serde_json::json!({ "section": saved.section, "before": saved.before })
    } else {
        serde_json::json!({ "section": saved.section, "index": saved.index })
    }
}

/// Ask the running shell to do something. `Ok(false)` when it answered but
/// did not say "ok".
fn shell_call(args: &[&str]) -> Result<bool> {
    Ok(shell_output(args)? == "ok")
}

/// What the running shell answers, or an error when it gave none.
///
/// Switching a plugin takes the shell seconds, longer than `omarchy-shell`
/// waits by default, and a call it gave up on may still go through later. So
/// it is given longer, and no answer is an error: never a refusal, which
/// would let a record go while the change is still on its way.
fn shell_output(args: &[&str]) -> Result<String> {
    let out = Command::new("omarchy-shell")
        .args(args)
        .env("OMARCHY_SHELL_IPC_TIMEOUT", "10s")
        .output()
        .context("running omarchy-shell")?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("omarchy-shell: ")
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Hide widget `id` from the bar, or put it back if the dock hid it.
///
/// Idempotent, so it can run on every config change: it only acts when the
/// bar and the wish disagree.
pub fn sync(id: &str, hide: bool) -> Result<()> {
    // Config changes can arrive in bursts; one reconciliation at a time.
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let (Some(shell_json), Some(record)) = (shell_json_path(), record_path(id)) else {
        anyhow::bail!("cannot find your config and state directories (is HOME set?)");
    };
    // The record is written and deleted by name, which is only the dock's to
    // do while its state directory is its own rather than a link elsewhere.
    if let Some(dir) = record.parent() {
        anyhow::ensure!(
            !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()),
            "{} is a symlink; not keeping the bar's widget record there",
            dir.display()
        );
    }
    reconcile(id, hide, &shell_json, &record, &mut shell_call)
}

/// What `sync` does once it knows where the files are, with `call` asking
/// the shell.
///
/// The answer can come back late or garbled while the change went through,
/// and the shell saves shell.json in the background, so an "ok" can arrive
/// before the file says so. So the record follows what shell.json says after
/// each call, given a moment to catch up with an "ok", not the reply: it is
/// kept while the widget is off the bar, and goes only once it is back.
/// Dropping it on a lost reply left the widget hidden with nothing to put it
/// back.
fn reconcile(
    id: &str,
    hide: bool,
    shell_json: &Path,
    record: &Path,
    call: &mut dyn FnMut(&[&str]) -> Result<bool>,
) -> Result<()> {
    let Some(shell) = read_shell(shell_json)? else {
        // No shell config: no bar to change.
        return Ok(());
    };
    let saved: Option<Placement> = std::fs::read_to_string(record)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());

    match (hide, locate(&shell, id), saved) {
        (true, Some(here), _) => {
            // Recorded first: if the shell then removes the widget but the
            // dock dies before noting it, the widget can still come back.
            crate::safe_write::replace(record, serde_json::to_string_pretty(&here)?.as_bytes())?;
            let answer = call(&["shell", "setPluginEnabled", id, "false"]);
            match settle(shell_json, id, false, &answer) {
                Some(false) => tracing::info!(id, ?here, "hid bar widget"),
                // It said it did: a record left over is harmless, as the
                // next sync drops it while the widget is on the bar.
                Some(true) if matches!(answer, Ok(true)) => {
                    anyhow::bail!("the shell said it disabled {id}, but it is still on the bar")
                }
                // No answer: it may still go through.
                Some(true) if answer.is_err() => {
                    answer?;
                }
                Some(true) => {
                    std::fs::remove_file(record).ok();
                    answer?;
                    anyhow::bail!("the shell would not disable {id}");
                }
                None => anyhow::bail!("cannot tell whether the shell disabled {id}; keeping its record"),
            }
        }
        (false, None, Some(saved)) => {
            let placement = restore_placement(&shell, &saved).to_string();
            let answer = call(&["shell", "enablePlugin", id, &placement]);
            if settle(shell_json, id, true, &answer) == Some(true) {
                std::fs::remove_file(record).ok();
                tracing::info!(id, %placement, "restored bar widget");
            } else {
                answer?;
                anyhow::bail!("the shell would not re-enable {id}");
            }
        }
        // The user put it back themselves while hiding was off; it is theirs
        // again, and the record is stale.
        (false, Some(_), Some(_)) => {
            std::fs::remove_file(record).ok();
        }
        _ => {}
    }
    Ok(())
}

/// shell.json, parsed. `None` when there is none.
fn read_shell(path: &Path) -> Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading shell.json"),
    };
    Ok(Some(serde_json::from_str(&text).context("parsing shell.json")?))
}

/// Whether widget `id` is on the bar, by shell.json, once the shell has had
/// a moment to save a change it answered "ok" to.
fn settle(shell_json: &Path, id: &str, want: bool, answer: &Result<bool>) -> Option<bool> {
    settled(|| on_bar(shell_json, id), want, matches!(answer, Ok(true)))
}

/// What `read` says, read again for a moment while it is not yet `want` and
/// the shell said "ok": the shell saves shell.json in the background.
fn settled(read: impl Fn() -> Option<bool>, want: bool, said_ok: bool) -> Option<bool> {
    let mut now = read();
    if said_ok {
        for _ in 0..20 {
            if now == Some(want) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            now = read();
        }
    }
    now
}

/// Whether widget `id` is on the bar now, by shell.json. `None` when that
/// cannot be told.
fn on_bar(shell_json: &Path, id: &str) -> Option<bool> {
    read_shell(shell_json).ok().flatten().map(|s| locate(&s, id).is_some())
}

// ── a shell plugin the dock stands in for ───────────────────────────────────
//
// Omarchy 4.5 brings a dock of its own, shown on floating workspaces. Two
// docks on screen is one too many, so the dock switches Omarchy's off while
// it runs, through the same plugin switch, and gives it back when it stops.
// A first-party plugin is on unless it is listed in `disabledPlugins`, so
// there is no placement to remember: the record only says the dock was the
// one that switched it off, so one the user switched off stays off.

/// Omarchy's own dock.
pub const OMARCHY_DOCK: &str = "omarchy.dock";

/// The record of a shell plugin the dock switched off.
fn off_record_path(id: &str) -> Option<PathBuf> {
    Some(dirs::state_dir()?.join("omarchy-dock").join(format!("switched-off-plugin-{id}.json")))
}

/// Switch shell plugin `id` off, or back on if the dock switched it off.
///
/// Idempotent, so it can run on every config change.
pub fn sync_plugin(id: &str, off: bool) -> Result<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let (Some(shell_json), Some(record)) = (shell_json_path(), off_record_path(id)) else {
        anyhow::bail!("cannot find your config and state directories (is HOME set?)");
    };
    if let Some(dir) = record.parent() {
        anyhow::ensure!(
            !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()),
            "{} is a symlink; not keeping the plugin record there",
            dir.display()
        );
    }
    reconcile_plugin(id, off, &shell_json, &record, &mut shell_output)
}

/// What `sync_plugin` does once it knows where the files are, with `call`
/// asking the shell. As with the bar widget, the record follows shell.json,
/// not the reply.
fn reconcile_plugin(
    id: &str,
    off: bool,
    shell_json: &Path,
    record: &Path,
    call: &mut dyn FnMut(&[&str]) -> Result<String>,
) -> Result<()> {
    let Some(disabled) = plugin_disabled(shell_json, id) else {
        // No shell config: no shell to change.
        return Ok(());
    };
    let ours = record.exists();

    match (off, disabled, ours) {
        (true, false, _) => {
            // The shell records any id it is handed, so only one it has.
            let listed: Value = serde_json::from_str(&call(&["shell", "listPlugins"])?)
                .context("reading the shell's plugin list")?;
            let known = listed
                .as_array()
                .is_some_and(|list| list.iter().any(|p| p.get("id").and_then(Value::as_str) == Some(id)));
            if !known {
                std::fs::remove_file(record).ok();
                return Ok(());
            }
            crate::safe_write::replace(record, serde_json::json!({ "id": id }).to_string().as_bytes())?;
            let said_ok = call(&["shell", "setPluginEnabled", id, "false"])? == "ok";
            match settled(|| plugin_disabled(shell_json, id), true, said_ok) {
                Some(true) => tracing::info!(id, "switched off the shell's plugin"),
                // It said it did: a record left over is harmless, as the
                // next sync drops it while the plugin is on.
                Some(false) if said_ok => anyhow::bail!("the shell said it switched {id} off, but it is on"),
                Some(false) => {
                    std::fs::remove_file(record).ok();
                    anyhow::bail!("the shell would not switch {id} off");
                }
                None => anyhow::bail!("cannot tell whether the shell switched {id} off; keeping its record"),
            }
        }
        (false, true, true) => {
            let said_ok = call(&["shell", "setPluginEnabled", id, "true"])? == "ok";
            if settled(|| plugin_disabled(shell_json, id), false, said_ok) == Some(false) {
                std::fs::remove_file(record).ok();
                tracing::info!(id, "switched the shell's plugin back on");
            } else {
                anyhow::bail!("the shell would not switch {id} back on");
            }
        }
        // Switched back on by hand while the dock had it off: it is the
        // user's again, and there is nothing to give back.
        (false, false, true) => {
            std::fs::remove_file(record).ok();
        }
        _ => {}
    }
    Ok(())
}

/// Whether shell plugin `id` is switched off in shell.json. `None` when that
/// cannot be told.
fn plugin_disabled(shell_json: &Path, id: &str) -> Option<bool> {
    let shell = read_shell(shell_json).ok().flatten()?;
    Some(shell.get("disabledPlugins").and_then(Value::as_array).is_some_and(|list| {
        list.iter().any(|e| e.as_str() == Some(id))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(left: &[&str], right: &[&str]) -> Value {
        let entries = |ids: &[&str]| -> Vec<Value> {
            ids.iter().map(|id| serde_json::json!({ "id": id })).collect()
        };
        serde_json::json!({ "bar": { "layout": {
            "left": entries(left), "center": [], "right": entries(right),
        }}})
    }

    #[test]
    fn a_widget_is_located_with_its_neighbours() {
        let shell = bar(&["omarchy.menu", "omarchy.workspaces", "argus"], &[]);
        let p = locate(&shell, WORKSPACES).unwrap();
        assert_eq!(p.section, "left");
        assert_eq!(p.index, 1);
        assert_eq!(p.after.as_deref(), Some("omarchy.menu"));
        assert_eq!(p.before.as_deref(), Some("argus"));
        assert!(locate(&shell, "missing").is_none());
    }

    #[test]
    fn it_goes_back_after_the_widget_it_followed() {
        let saved = locate(&bar(&["menu", "omarchy.workspaces", "argus"], &[]), WORKSPACES).unwrap();
        let now = bar(&["menu", "argus"], &[]);
        assert_eq!(
            restore_placement(&now, &saved),
            serde_json::json!({ "section": "left", "after": "menu" })
        );
    }

    #[test]
    fn it_falls_back_to_the_next_widget_then_to_the_index() {
        let saved = locate(&bar(&["menu", "omarchy.workspaces", "argus"], &[]), WORKSPACES).unwrap();
        // The widget before it is gone, the one after remains.
        assert_eq!(
            restore_placement(&bar(&["argus"], &[]), &saved),
            serde_json::json!({ "section": "left", "before": "argus" })
        );
        // Both gone, or moved to another section: the old index.
        assert_eq!(
            restore_placement(&bar(&["other"], &["menu", "argus"]), &saved),
            serde_json::json!({ "section": "left", "index": 1 })
        );
    }

    /// A throwaway shell.json holding `left`, and where the record goes.
    fn scratch(tag: &str, left: &[&str]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("omarchy-dock-bar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("shell.json");
        std::fs::write(&shell, bar(left, &[]).to_string()).unwrap();
        (shell, dir.join("record.json"))
    }

    /// A shell that lays the bar out as `left` and then answers `reply`.
    fn shell_that(shell: &std::path::Path, left: &[&str], reply: bool) -> impl FnMut(&[&str]) -> Result<bool> {
        let (shell, layout) = (shell.to_path_buf(), bar(left, &[]).to_string());
        move |_| {
            std::fs::write(&shell, &layout).unwrap();
            Ok(reply)
        }
    }

    #[test]
    fn a_widget_the_shell_hid_keeps_its_record_whatever_the_shell_answered() {
        // The shell writes shell.json and then answers; a reply that comes
        // back garbled or late must not cost the record of where it sat.
        let (shell, record) = scratch("hid", &["menu", WORKSPACES]);
        reconcile(WORKSPACES, true, &shell, &record, &mut shell_that(&shell, &["menu"], false)).unwrap();
        assert!(record.exists(), "the widget is off the bar: keep the record");

        // So turning hiding off still brings it back.
        reconcile(WORKSPACES, false, &shell, &record, &mut shell_that(&shell, &["menu", WORKSPACES], true)).unwrap();
        assert!(!record.exists());
    }

    /// A shell that answers "ok" at once and lays the bar out as `left` a
    /// moment later: it saves shell.json in the background.
    fn shell_that_saves_late(shell: &std::path::Path, left: &[&str]) -> impl FnMut(&[&str]) -> Result<bool> {
        let (shell, layout) = (shell.to_path_buf(), bar(left, &[]).to_string());
        move |_| {
            let (shell, layout) = (shell.clone(), layout.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                std::fs::write(&shell, &layout).unwrap();
            });
            Ok(true)
        }
    }

    #[test]
    fn a_shell_that_saves_after_it_answers_is_waited_for() {
        let (shell, record) = scratch("late", &["menu", WORKSPACES]);
        reconcile(WORKSPACES, true, &shell, &record, &mut shell_that_saves_late(&shell, &["menu"])).unwrap();
        assert!(record.exists(), "hidden, once the shell has saved: keep the record");

        reconcile(WORKSPACES, false, &shell, &record, &mut shell_that_saves_late(&shell, &["menu", WORKSPACES]))
            .unwrap();
        assert!(!record.exists());
    }

    #[test]
    fn a_shell_that_never_answers_still_leaves_the_record() {
        // Out of time, the change may still land: keep what puts it back.
        let (shell, record) = scratch("silent", &["menu", WORKSPACES]);
        let late = shell_that_saves_late(&shell, &["menu"]);
        let mut silent = {
            let mut late = late;
            move |args: &[&str]| -> Result<bool> {
                let _ = late(args);
                anyhow::bail!("omarchy-shell is not responding")
            }
        };
        assert!(reconcile(WORKSPACES, true, &shell, &record, &mut silent).is_err());
        assert!(record.exists());
        std::thread::sleep(std::time::Duration::from_millis(300));
        // So once it has landed, turning hiding off still brings it back.
        reconcile(WORKSPACES, false, &shell, &record, &mut shell_that(&shell, &["menu", WORKSPACES], true)).unwrap();
        assert!(!record.exists());
    }

    #[test]
    fn a_widget_the_shell_left_on_the_bar_drops_its_record() {
        let (shell, record) = scratch("refused", &["menu", WORKSPACES]);
        let left = &["menu", WORKSPACES];
        assert!(reconcile(WORKSPACES, true, &shell, &record, &mut shell_that(&shell, left, false)).is_err());
        assert!(!record.exists());
    }

    #[test]
    fn the_record_goes_only_once_the_widget_is_back() {
        // The shell says "ok" but the widget is not on the bar: keep trying.
        let (shell, record) = scratch("restore", &["menu"]);
        std::fs::write(&record, r#"{"section":"left","index":1,"after":"menu","before":null}"#).unwrap();
        assert!(reconcile(WORKSPACES, false, &shell, &record, &mut shell_that(&shell, &["menu"], true)).is_err());
        assert!(record.exists());
        // It came back though the reply was lost: done.
        reconcile(WORKSPACES, false, &shell, &record, &mut shell_that(&shell, &["menu", WORKSPACES], false)).unwrap();
        assert!(!record.exists());
    }

    #[test]
    fn plain_string_entries_are_understood() {
        let shell = serde_json::json!({ "bar": { "layout": { "left": ["a", "omarchy.workspaces"] } } });
        assert_eq!(locate(&shell, WORKSPACES).unwrap().after.as_deref(), Some("a"));
    }

    // ── a shell plugin the dock stands in for ──────────────────────────────

    /// A throwaway shell.json with `disabled` in `disabledPlugins`.
    fn plugins_scratch(tag: &str, disabled: &[&str]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("omarchy-dock-plugin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("shell.json");
        std::fs::write(&shell, serde_json::json!({ "plugins": [], "disabledPlugins": disabled }).to_string())
            .unwrap();
        (shell, dir.join("record.json"))
    }

    /// A shell that has Omarchy's dock when `known`, switches plugins as
    /// asked (saving `delay_ms` after it answers), and counts the switches.
    fn plugin_shell(
        shell: &std::path::Path,
        known: bool,
        delay_ms: u64,
        switches: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> impl FnMut(&[&str]) -> Result<String> {
        let shell = shell.to_path_buf();
        move |args| match args {
            ["shell", "listPlugins"] => {
                let ids: &[&str] = if known { &[OMARCHY_DOCK, "omarchy.bar"] } else { &["omarchy.bar"] };
                Ok(serde_json::json!(ids.iter().map(|id| serde_json::json!({ "id": id })).collect::<Vec<_>>())
                    .to_string())
            }
            ["shell", "setPluginEnabled", id, on] => {
                switches.set(switches.get() + 1);
                let (shell, id, on) = (shell.clone(), id.to_string(), *on == "true");
                let save = move || {
                    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&shell).unwrap()).unwrap();
                    let list = v["disabledPlugins"].as_array_mut().unwrap();
                    list.retain(|e| e.as_str() != Some(id.as_str()));
                    if !on {
                        list.push(Value::from(id));
                    }
                    std::fs::write(&shell, v.to_string()).unwrap();
                };
                if delay_ms == 0 {
                    save();
                } else {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        save();
                    });
                }
                Ok("ok".into())
            }
            _ => panic!("unexpected shell call {args:?}"),
        }
    }

    fn switched_off(shell: &std::path::Path) -> bool {
        plugin_disabled(shell, OMARCHY_DOCK) == Some(true)
    }

    #[test]
    fn omarchys_dock_is_switched_off_and_given_back() {
        let (shell, record) = plugins_scratch("swap", &["omarchy.lock"]);
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        reconcile_plugin(OMARCHY_DOCK, true, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(switched_off(&shell));
        assert!(record.exists(), "the dock switched it off: remember that");

        reconcile_plugin(OMARCHY_DOCK, false, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(!switched_off(&shell));
        assert!(!record.exists());
        // The user's own choices are left as they were.
        assert_eq!(plugin_disabled(&shell, "omarchy.lock"), Some(true));
    }

    #[test]
    fn a_dock_the_user_switched_off_is_never_switched_back_on() {
        let (shell, record) = plugins_scratch("users", &[OMARCHY_DOCK]);
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        reconcile_plugin(OMARCHY_DOCK, true, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(!record.exists(), "already off, and not by the dock");
        reconcile_plugin(OMARCHY_DOCK, false, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(switched_off(&shell));
        assert_eq!(n.get(), 0);
    }

    #[test]
    fn an_omarchy_without_a_dock_of_its_own_is_left_alone() {
        let (shell, record) = plugins_scratch("none", &[]);
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        reconcile_plugin(OMARCHY_DOCK, true, &shell, &record, &mut plugin_shell(&shell, false, 0, n.clone()))
            .unwrap();
        assert!(!switched_off(&shell), "the shell records any id it is handed: none is");
        assert!(!record.exists());
        assert_eq!(n.get(), 0);
    }

    #[test]
    fn a_shell_that_saves_late_is_waited_for_both_ways() {
        let (shell, record) = plugins_scratch("late-plugin", &[]);
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        reconcile_plugin(OMARCHY_DOCK, true, &shell, &record, &mut plugin_shell(&shell, true, 150, n.clone()))
            .unwrap();
        assert!(switched_off(&shell) && record.exists());
        reconcile_plugin(OMARCHY_DOCK, false, &shell, &record, &mut plugin_shell(&shell, true, 150, n.clone()))
            .unwrap();
        assert!(!switched_off(&shell) && !record.exists());
    }

    #[test]
    fn a_switch_the_shell_never_answered_keeps_its_record() {
        let (shell, record) = plugins_scratch("silent-plugin", &[]);
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut late = plugin_shell(&shell, true, 150, n.clone());
        let mut silent = move |args: &[&str]| -> Result<String> {
            let answer = late(args)?;
            if args.get(1) == Some(&"setPluginEnabled") {
                anyhow::bail!("omarchy-shell is not responding")
            }
            Ok(answer)
        };
        assert!(reconcile_plugin(OMARCHY_DOCK, true, &shell, &record, &mut silent).is_err());
        assert!(record.exists(), "it may have gone through: keep what gives it back");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(switched_off(&shell));
        reconcile_plugin(OMARCHY_DOCK, false, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(!switched_off(&shell) && !record.exists());
    }

    #[test]
    fn a_dock_the_user_switched_back_on_drops_the_record() {
        // Turned back on by hand while ours ran: it is theirs again, and
        // turning the setting off has nothing to give back.
        let (shell, record) = plugins_scratch("back-on", &[]);
        std::fs::write(&record, "{}").unwrap();
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        reconcile_plugin(OMARCHY_DOCK, false, &shell, &record, &mut plugin_shell(&shell, true, 0, n.clone()))
            .unwrap();
        assert!(!record.exists());
        assert_eq!(n.get(), 0);
    }
}
