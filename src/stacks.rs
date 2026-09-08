//! Folder stacks and Trash.
//!
//! Directory listing is deliberately synchronous and bounded: a stack shows a
//! handful of recent entries, opened from a click, so the cost is a single
//! readdir of one directory rather than anything worth pushing onto the async
//! worker.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many entries a stack shows before "Open folder" takes over.
pub const STACK_LIMIT: usize = 12;

#[derive(Debug, Clone)]
pub struct StackEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
}

/// The freedesktop trash directory holding trashed files themselves.
pub fn trash_files_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Trash")
        .join("files")
}

/// Whether the trash has anything in it. Drives the empty/full icon.
pub fn trash_is_empty() -> bool {
    std::fs::read_dir(trash_files_dir())
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// Most recently modified entries in a directory, newest first.
///
/// Hidden files are skipped: a stack is a shortcut to recent work, and dotfiles
/// are almost never that.
pub fn recent(dir: &Path, limit: usize) -> Vec<StackEntry> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };

    let mut entries: Vec<StackEntry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let meta = e.metadata().ok()?;
            Some(StackEntry {
                path: e.path(),
                name,
                is_dir: meta.is_dir(),
                modified: meta.modified().ok(),
            })
        })
        .collect();

    // Newest first; entries without a timestamp sort last rather than
    // poisoning the comparison.
    entries.sort_by(|a, b| match (a.modified, b.modified) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    entries.truncate(limit);
    entries
}

/// Open a path with the desktop's handler.
pub fn open(path: &Path) {
    spawn("xdg-open", &[path.as_os_str()]);
}

/// Reveal a path in the file manager.
///
/// `--select` is a Nautilus/Dolphin convention; when the handler does not
/// understand it the parent directory is still a sensible result, so falling
/// back to opening the parent is better than doing nothing.
pub fn reveal(path: &Path) {
    if let Some(parent) = path.parent() {
        spawn("xdg-open", &[parent.as_os_str()]);
    }
}

/// Move a path to the trash via GIO, which implements the freedesktop spec
/// (including the `.trashinfo` bookkeeping that makes "restore" work).
pub fn trash(path: &Path) -> bool {
    use gtk4::gio;
    use gtk4::gio::prelude::FileExt;
    let file = gio::File::for_path(path);
    match file.trash(gio::Cancellable::NONE) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot trash");
            false
        }
    }
}

/// Permanently delete everything in the trash.
///
/// Deletes the trashed files and their `.trashinfo` records together; leaving
/// the records behind would show phantom entries in every file manager.
pub fn empty_trash() -> usize {
    let mut removed = 0;
    for dir in [trash_files_dir(), trash_info_dir()] {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            let ok = if path.is_dir() && !path.is_symlink() {
                std::fs::remove_dir_all(&path).is_ok()
            } else {
                std::fs::remove_file(&path).is_ok()
            };
            if ok {
                removed += 1;
            } else {
                tracing::warn!(path = %path.display(), "cannot remove from trash");
            }
        }
    }
    removed
}

fn trash_info_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Trash")
        .join("info")
}

fn spawn(program: &str, args: &[&std::ffi::OsStr]) {
    match std::process::Command::new(program).args(args).spawn() {
        Ok(_) => {}
        Err(e) => tracing::warn!(program, error = %e, "cannot launch"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_lists_newest_first_and_skips_dotfiles() {
        let dir = std::env::temp_dir().join(format!("omarchy-dock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["old.txt", ".hidden", "new.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
            // Ensure distinct mtimes regardless of filesystem granularity.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let got = recent(&dir, 10);
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["new.txt", "old.txt"], "dotfiles excluded, newest first");

        assert_eq!(recent(&dir, 1).len(), 1, "limit honoured");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_is_empty_rather_than_an_error() {
        assert!(recent(Path::new("/nonexistent/omarchy-dock"), 5).is_empty());
    }
}
