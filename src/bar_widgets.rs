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
    let out = Command::new("omarchy-shell")
        .args(args)
        .output()
        .context("running omarchy-shell")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "ok")
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
    let mut now = on_bar(shell_json, id);
    if matches!(answer, Ok(true)) {
        for _ in 0..20 {
            if now == Some(want) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            now = on_bar(shell_json, id);
        }
    }
    now
}

/// Whether widget `id` is on the bar now, by shell.json. `None` when that
/// cannot be told.
fn on_bar(shell_json: &Path, id: &str) -> Option<bool> {
    read_shell(shell_json).ok().flatten().map(|s| locate(&s, id).is_some())
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
}
