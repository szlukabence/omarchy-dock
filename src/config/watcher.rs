//! Debounced filesystem watching for live config and theme reload.
//!
//! Four things are watched:
//!   * `~/.config/omarchy-dock/`            — config.toml and style.css
//!   * `~/.local/state/omarchy/current/`    — the active-theme symlink
//!   * the theme directory it resolves to   — in-place palette edits
//!   * `/tmp` for Omarchy's screen-recording marker — so the dock can get out
//!     of the way of a recording, and come back when it stops
//!
//! Editors rarely write a file once; they truncate, write, rename, and often
//! touch a backup alongside. Debouncing collapses that into a single reload.

use crate::event::{AppEvent, Sender};
use crate::theme;
use anyhow::Result;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebouncedEvent};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(150);

/// Start watching. The returned handle must be kept alive; dropping it stops
/// the watch. Runs its own thread and never touches GTK.
pub fn spawn(tx: Sender) -> Result<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("omarchy-dock-watcher".into())
        .spawn(move || {
            if let Err(e) = run(tx) {
                tracing::error!(error = %e, "file watcher stopped");
            }
        })?;
    Ok(handle)
}

fn run(tx: Sender) -> Result<()> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(DEBOUNCE, None, raw_tx)?;

    let config_dir = crate::config::config_dir();
    // The config directory may not exist yet on a first run.
    std::fs::create_dir_all(&config_dir).ok();

    let state_dir = theme::state_dir();
    let mut theme_dir = theme::current_theme_dir();

    debouncer.watch(&config_dir, RecursiveMode::NonRecursive)?;
    // The marker's *directory*, because the file itself does not exist most of
    // the time and notify cannot watch a path that is not there yet.
    if let Some(dir) = crate::omarchy::RECORDING_MARKER.parent() {
        if dir.exists() {
            debouncer.watch(dir, RecursiveMode::NonRecursive).ok();
        }
    }
    if state_dir.exists() {
        debouncer.watch(&state_dir, RecursiveMode::NonRecursive)?;
    }
    if let Some(d) = &theme_dir {
        debouncer.watch(d, RecursiveMode::NonRecursive)?;
    }

    tracing::info!(
        config = %config_dir.display(),
        state = %state_dir.display(),
        theme = ?theme_dir.as_ref().map(|d| d.display().to_string()),
        "watching for changes"
    );

    for result in raw_rx {
        let events: Vec<DebouncedEvent> = match result {
            Ok(events) => events,
            Err(errors) => {
                for e in errors {
                    tracing::warn!(error = %e, "watch error");
                }
                continue;
            }
        };

        // Reading a file emits inotify IN_ACCESS. Since reloading *reads*
        // config.toml, reacting to access events makes the dock retrigger
        // itself forever. Only real mutations count.
        let paths: Vec<&Path> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            })
            .flat_map(|e| e.paths.iter().map(PathBuf::as_path))
            .collect();

        if paths.is_empty() {
            continue;
        }

        // A theme switch replaces the symlink, so re-point the watch at the
        // new directory or in-place palette edits would go unnoticed.
        let latest = theme::current_theme_dir();
        if latest != theme_dir {
            if let Some(old) = &theme_dir {
                debouncer.unwatch(old).ok();
            }
            if let Some(new) = &latest {
                if let Err(e) = debouncer.watch(new, RecursiveMode::NonRecursive) {
                    tracing::warn!(error = %e, "cannot watch new theme dir");
                }
            }
            theme_dir = latest;
        }

        let config_changed = paths.iter().any(|p| p.ends_with("config.toml"));
        let style_changed = paths.iter().any(|p| {
            p.ends_with("style.css")
                || p.ends_with("colors.toml")
                || p.ends_with("theme.name")
                || p.ends_with("theme")
                || p.ends_with("icons.theme")
        });

        let recording_changed =
            paths.iter().any(|p| *p == crate::omarchy::RECORDING_MARKER.as_path());

        // Config first: a rebuild restyles anyway, so sending both would
        // duplicate the work.
        let event = if config_changed {
            Some(AppEvent::ConfigChanged)
        } else if style_changed {
            Some(AppEvent::StyleChanged)
        } else if recording_changed {
            Some(AppEvent::HidePolicyChanged)
        } else {
            None
        };

        if let Some(event) = event {
            tracing::debug!(?event, "reload triggered");
            // Closed channel means the GTK side is gone; stop watching.
            if tx.send_blocking(event).is_err() {
                break;
            }
        }
    }

    Ok(())
}
