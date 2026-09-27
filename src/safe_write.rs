//! Replacing a file's contents without a moment in which it is truncated.
//!
//! Shared by the dock (its config) and `omarchy-dockctl` (the Omarchy files it
//! edits), so both go through the same rule.

use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Replace `path`'s contents in one step: the new contents go to a temporary
/// file beside it, are flushed to disk, and only then renamed over it. A crash,
/// a full disk or a failed write leaves the old file whole rather than
/// truncated. A symlink (a dotfiles checkout, say) is followed, so the link
/// stays and its target is what changes; an existing file keeps its mode.
pub fn replace(path: &Path, contents: &[u8]) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        anyhow::bail!("{} is not a file path", path.display());
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mode = std::fs::metadata(&target).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o644);

    let tmp = dir.join(format!(".{}.omarchy-dock-{}", name.to_string_lossy(), std::process::id()));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(contents)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("omarchy-dock-safe-write-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_contents_and_keeps_the_mode() {
        let dir = scratch("mode");
        let file = dir.join("f");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        replace(&file, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // Nothing left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_symlink_stays_a_symlink() {
        let dir = scratch("link");
        let real = dir.join("real");
        let link = dir.join("link");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn a_failed_write_leaves_the_old_file() {
        let dir = scratch("fail");
        let file = dir.join("f");
        std::fs::write(&file, "old").unwrap();
        // The temporary file cannot be created in a read-only directory.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = replace(&file, b"new");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
    }
}
