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

/// Suffixes a browser gives a download until it completes: `.crdownload` for
/// Chromium, Chrome, Edge and friends, `.part` for Firefox.
const PARTIAL: [&str; 2] = ["crdownload", "part"];

/// Whether `path` is a download still in progress.
pub fn is_partial(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| PARTIAL.contains(&e))
}

/// How many downloads are in progress in `dir`.
///
/// Counted from the browsers' temporary files. How far along each one is,
/// nobody says: the file grows, but its final size is known only to the
/// browser.
pub fn active_downloads(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter(|e| is_partial(&e.path())).count())
        .unwrap_or(0)
}

/// The freedesktop trash directory holding trashed files themselves.
///
/// Without a data directory there is no trash: the path then names nothing,
/// rather than a directory other users can write to.
pub fn trash_files_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
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

/// Permanently delete one item in the trash, and its `.trashinfo` record with
/// it so no file manager shows a phantom entry.
///
/// Only a trashed item is deleted: a direct child of the trash's `files`
/// directory with a valid `.trashinfo` record in `info` — the record every
/// trash implementation writes when it trashes something. Anything else is
/// refused, wherever the trash directory turns out to live.
pub fn delete_from_trash(path: &Path) -> bool {
    trash_dirs().is_some_and(|(files, info)| delete_from_trash_in(&files, &info, path))
}

fn delete_from_trash_in(files: &Path, info: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name() else { return false };
    if path.parent() != Some(files) {
        tracing::warn!(path = %path.display(), "not in the trash; not deleting");
        return false;
    }
    let record = record_for(info, name);
    if !is_trash_record(&record) {
        tracing::warn!(path = %path.display(), "no trash record for it; not deleting");
        return false;
    }
    let ok = remove_entry(path);
    if ok {
        std::fs::remove_file(&record).ok();
    } else {
        tracing::warn!(path = %path.display(), "cannot delete from trash");
    }
    ok
}

/// What emptying the trash did.
pub struct Emptied {
    /// Trashed items deleted, each with its record.
    pub deleted: usize,
    /// Entries left because nothing proves they are trash: a file in `files`
    /// with no record, or something in `info` that is not a record.
    pub kept: usize,
}

/// Permanently delete everything in the trash.
///
/// Walks the records, not the files: each valid `.trashinfo` names one
/// trashed item, which goes together with its record (a record whose item is
/// already gone goes too, or file managers would show a phantom entry).
/// Whatever has no record is left, since nothing says it is trash.
pub fn empty_trash() -> Emptied {
    let Some((files, info)) = trash_dirs() else { return Emptied { deleted: 0, kept: 0 } };
    empty_trash_in(&files, &info)
}

fn empty_trash_in(files: &Path, info: &Path) -> Emptied {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Emptied { deleted: 0, kept: 0 };
    if let Ok(read) = std::fs::read_dir(info) {
        for entry in read.flatten() {
            let record = entry.path();
            let name = entry.file_name();
            let Some(item) = name.as_bytes().strip_suffix(b".trashinfo") else { continue };
            if item.is_empty() || !is_trash_record(&record) {
                continue;
            }
            let item = files.join(std::ffi::OsStr::from_bytes(item));
            let gone = std::fs::symlink_metadata(&item).is_err() || remove_entry(&item);
            if gone && std::fs::remove_file(&record).is_ok() {
                out.deleted += 1;
            } else {
                tracing::warn!(path = %item.display(), "cannot remove from trash");
            }
        }
    }
    let left = |dir: &Path| std::fs::read_dir(dir).map(|r| r.flatten().count()).unwrap_or(0);
    out.kept = left(files) + left(info);
    if out.kept > 0 {
        tracing::warn!(kept = out.kept, "left entries in the trash that have no trash record");
    }
    out
}

/// Delete a trash entry: a directory with everything in it, anything else
/// (a symlink included, never what it points at) on its own.
fn remove_entry(path: &Path) -> bool {
    let real_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir());
    if real_dir { std::fs::remove_dir_all(path).is_ok() } else { std::fs::remove_file(path).is_ok() }
}

fn record_for(info: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let mut record = name.to_os_string();
    record.push(".trashinfo");
    info.join(record)
}

/// Whether `path` is a `.trashinfo` record as the freedesktop trash spec
/// defines it: a regular file (not a symlink) opening with `[Trash Info]`
/// and giving the item's original `Path=` and its `DeletionDate=`.
fn is_trash_record(path: &Path) -> bool {
    use std::io::Read;
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
        return false;
    }
    // A record is a few lines; read no more than a record could be.
    let mut head = Vec::new();
    let Ok(file) = std::fs::File::open(path) else { return false };
    if file.take(64 * 1024).read_to_end(&mut head).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines().map(str::trim);
    lines.next() == Some("[Trash Info]")
        && text.lines().any(|l| l.starts_with("Path="))
        && text.lines().any(|l| l.starts_with("DeletionDate="))
}

/// The trash's `files` and `info` directories, for deleting from. Unlike
/// [`trash_files_dir`], which only reads, there is no fallback: without a
/// data directory there is no trash, and nothing to delete.
///
/// `Trash`, `files` and `info` must each be a real directory. A symlink at
/// any of them points somewhere other than the trash, and "emptying the
/// trash" would then mean deleting from wherever it points. (Every entry is
/// also checked against its record before it goes, so even a data directory
/// that is itself linked elsewhere loses nothing but trashed items.)
fn trash_dirs() -> Option<(PathBuf, PathBuf)> {
    trash_dirs_in(&dirs::data_dir()?)
}

fn trash_dirs_in(data: &Path) -> Option<(PathBuf, PathBuf)> {
    let trash = data.join("Trash");
    let (files, info) = (trash.join("files"), trash.join("info"));
    for dir in [&trash, &files, &info] {
        if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
            tracing::warn!(path = %dir.display(), "not a real trash directory; not deleting from it");
            return None;
        }
    }
    Some((files, info))
}

fn spawn(program: &str, args: &[&std::ffi::OsStr]) {
    match std::process::Command::new(program).args(args).spawn() {
        Ok(_) => {}
        Err(e) => tracing::warn!(program, error = %e, "cannot launch"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn downloads_in_progress_are_counted_by_their_browsers_suffix() {
        let dir = std::env::temp_dir().join(format!("omarchy-dock-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["done.zip", "Unconfirmed 1234.crdownload", "video.mp4.part", "notes.txt"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        assert_eq!(super::active_downloads(&dir), 2);
        assert!(super::is_partial(std::path::Path::new("a.crdownload")));
        assert!(!super::is_partial(std::path::Path::new("partial.zip")));
        std::fs::remove_dir_all(&dir).ok();
        // A folder that is not there has nothing downloading.
        assert_eq!(super::active_downloads(&dir), 0);
    }

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

    const RECORD: &[u8] = b"[Trash Info]\nPath=/home/u/a.txt\nDeletionDate=2026-09-27T12:00:00\n";

    fn scratch_trash(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir()
            .join(format!("omarchy-dock-trash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (files, info) = (root.join("files"), root.join("info"));
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        (root, files, info)
    }

    #[test]
    fn deleting_from_the_trash_takes_the_record_and_nothing_outside() {
        let (root, files, info) = scratch_trash("one");
        std::fs::create_dir_all(files.join("folder/inner")).unwrap();
        std::fs::write(files.join("a.txt"), b"x").unwrap();
        std::fs::write(info.join("a.txt.trashinfo"), RECORD).unwrap();
        std::fs::write(info.join("folder.trashinfo"), RECORD).unwrap();
        std::fs::write(root.join("outside.txt"), b"x").unwrap();

        assert!(delete_from_trash_in(&files, &info, &files.join("a.txt")));
        assert!(!files.join("a.txt").exists() && !info.join("a.txt.trashinfo").exists());
        assert!(delete_from_trash_in(&files, &info, &files.join("folder")));
        assert!(!files.join("folder").exists() && !info.join("folder.trashinfo").exists());

        // Only direct children of the trash's files directory.
        assert!(!delete_from_trash_in(&files, &info, &root.join("outside.txt")));
        assert!(!delete_from_trash_in(&files, &info, &files));
        assert!(root.join("outside.txt").exists() && files.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_entries_with_a_trash_record_are_deleted() {
        let (_root, files, info) = scratch_trash("records");
        // Trashed properly: goes.
        std::fs::write(files.join("trashed.txt"), b"x").unwrap();
        std::fs::write(info.join("trashed.txt.trashinfo"), RECORD).unwrap();
        // No record, or one that is not a trash record: stays.
        std::fs::write(files.join("unrecorded.txt"), b"x").unwrap();
        std::fs::write(files.join("fake.txt"), b"x").unwrap();
        std::fs::write(info.join("fake.txt.trashinfo"), b"not a record").unwrap();
        // A record whose item is already gone: the record goes.
        std::fs::write(info.join("orphan.trashinfo"), RECORD).unwrap();

        assert!(!delete_from_trash_in(&files, &info, &files.join("unrecorded.txt")));
        assert!(!delete_from_trash_in(&files, &info, &files.join("fake.txt")));

        let emptied = empty_trash_in(&files, &info);
        assert_eq!(emptied.deleted, 2);
        assert_eq!(emptied.kept, 3);
        assert!(!files.join("trashed.txt").exists() && !info.join("orphan.trashinfo").exists());
        assert!(files.join("unrecorded.txt").exists() && files.join("fake.txt").exists());
        assert!(info.join("fake.txt.trashinfo").exists());
    }

    #[test]
    fn a_trashed_symlink_goes_but_not_what_it_points_at() {
        let (root, files, info) = scratch_trash("link");
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/keep.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), files.join("link")).unwrap();
        std::fs::write(info.join("link.trashinfo"), RECORD).unwrap();
        assert_eq!(empty_trash_in(&files, &info).deleted, 1);
        assert!(root.join("elsewhere/keep.txt").exists());
    }

    #[test]
    fn a_linked_trash_directory_is_not_deleted_from() {
        let base = std::env::temp_dir()
            .join(format!("omarchy-dock-linked-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (data, target) = (base.join("data"), base.join("target"));
        std::fs::create_dir_all(target.join("files")).unwrap();
        std::fs::create_dir_all(target.join("info")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::os::unix::fs::symlink(&target, data.join("Trash")).unwrap();
        assert!(trash_dirs_in(&data).is_none());
        // The same trash, really there, is.
        std::fs::remove_file(data.join("Trash")).unwrap();
        std::fs::rename(&target, data.join("Trash")).unwrap();
        assert!(trash_dirs_in(&data).is_some());
        // And a symlinked `files` inside a real `Trash` is refused too.
        std::fs::remove_dir(data.join("Trash/files")).unwrap();
        std::os::unix::fs::symlink(&base, data.join("Trash/files")).unwrap();
        assert!(trash_dirs_in(&data).is_none());
    }

    #[test]
    fn a_missing_directory_is_empty_rather_than_an_error() {
        assert!(recent(Path::new("/nonexistent/omarchy-dock"), 5).is_empty());
    }
}
