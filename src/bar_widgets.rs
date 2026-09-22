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
use std::path::PathBuf;
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

fn shell_json_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omarchy/shell.json")
}

/// The record of a widget the dock has hidden.
fn record_path(id: &str) -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omarchy-dock")
        .join(format!("hidden-bar-widget-{id}.json"))
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

    let Ok(text) = std::fs::read_to_string(shell_json_path()) else {
        // No shell config: no bar to change.
        return Ok(());
    };
    let shell: Value = serde_json::from_str(&text).context("parsing shell.json")?;
    let record = record_path(id);
    let saved: Option<Placement> = std::fs::read_to_string(&record)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());

    match (hide, locate(&shell, id), saved) {
        (true, Some(here), _) => {
            // Recorded first: if the shell then removes the widget but the
            // dock dies before noting it, the widget can still come back.
            if let Some(dir) = record.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&record, serde_json::to_string_pretty(&here)?)?;
            if !shell_call(&["shell", "setPluginEnabled", id, "false"])? {
                std::fs::remove_file(&record).ok();
                anyhow::bail!("the shell would not disable {id}");
            }
            tracing::info!(id, ?here, "hid bar widget");
        }
        (false, None, Some(saved)) => {
            let placement = restore_placement(&shell, &saved).to_string();
            if shell_call(&["shell", "enablePlugin", id, &placement])? {
                std::fs::remove_file(&record).ok();
                tracing::info!(id, %placement, "restored bar widget");
            } else {
                anyhow::bail!("the shell would not re-enable {id}");
            }
        }
        // The user put it back themselves while hiding was off; it is theirs
        // again, and the record is stale.
        (false, Some(_), Some(_)) => {
            std::fs::remove_file(&record).ok();
        }
        _ => {}
    }
    Ok(())
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

    #[test]
    fn plain_string_entries_are_understood() {
        let shell = serde_json::json!({ "bar": { "layout": { "left": ["a", "omarchy.workspaces"] } } });
        assert_eq!(locate(&shell, WORKSPACES).unwrap().after.as_deref(), Some("a"));
    }
}
