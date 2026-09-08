//! Freedesktop `.desktop` entry scanning.
//!
//! gio-rs does not bind `GDesktopAppInfo` (it lives in gio-unix), and the
//! generic `AppInfo` exposes neither `StartupWMClass` nor `Actions`, both of
//! which the dock needs — the first to match windows, the second for
//! context menus. So entries are parsed directly.

// `command()`, `actions` and `path` feed the context menus and launching in
// Phase 5.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single `Desktop Action` group, surfaced in the right-click menu.
#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub name: String,
    pub exec: String,
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Desktop id without the `.desktop` suffix, e.g. `org.gnome.Nautilus`.
    pub id: String,
    pub name: String,
    pub icon: String,
    pub exec: String,
    /// `StartupWMClass`, when the app declares one.
    pub wm_class: Option<String>,
    pub no_display: bool,
    pub terminal: bool,
    pub actions: Vec<Action>,
    pub path: PathBuf,
}

impl Entry {
    /// The command to run, with field codes stripped.
    ///
    /// `%f %F %u %U %i %c %k` are launcher placeholders, not arguments; passing
    /// them through makes apps open files literally named "%U".
    pub fn command(&self) -> String {
        strip_field_codes(&self.exec)
    }
}

/// Reject `StartupWMClass` values that cannot be a real window class.
///
/// Arch's `chromium.desktop` ships `StartupWMClass=@@startup_wm_class` — an
/// unsubstituted build template. Treating that as authoritative made pinned
/// Chromium never match its own window, so the key is validated rather than
/// trusted.
fn sane_wm_class(value: &str) -> Option<String> {
    let v = value.trim();
    let bogus = v.is_empty()
        || v.contains("@@")
        || v.contains("${")
        || v.starts_with('@')
        || v.contains(char::is_whitespace);
    (!bogus).then(|| v.to_string())
}

pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                // `%%` is a literal percent.
                Some('%') => {
                    out.push('%');
                    chars.next();
                }
                Some(f) if "fFuUickdDnNvm".contains(*f) => {
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Directories searched for desktop entries, in precedence order.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::data_dir() {
        dirs.push(home.join("applications"));
    }
    let xdg = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in xdg.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d).join("applications"));
    }
    dirs
}

/// Scan all entries. Earlier directories win, matching XDG precedence, so a
/// user override in `~/.local/share/applications` shadows the system copy.
pub fn scan() -> Vec<Entry> {
    let mut seen: HashMap<String, Entry> = HashMap::new();

    for dir in search_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for item in rd.flatten() {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(entry) = parse(&path) else { continue };
            seen.entry(entry.id.clone()).or_insert(entry);
        }
    }

    let mut v: Vec<Entry> = seen.into_values().collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// Parse one `.desktop` file.
///
/// Only `[Desktop Entry]` and `[Desktop Action *]` groups are read; other
/// groups are skipped so unrelated keys cannot leak into the entry.
pub fn parse(path: &Path) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let id = path.file_stem()?.to_string_lossy().to_string();

    let mut entry = Entry {
        id,
        name: String::new(),
        icon: String::new(),
        exec: String::new(),
        wm_class: None,
        no_display: false,
        terminal: false,
        actions: Vec::new(),
        path: path.to_path_buf(),
    };

    // `None` = not in a group we care about.
    enum Group {
        Main,
        Action(usize),
        Other,
    }
    let mut group = Group::Other;
    let mut action_ids: Vec<String> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            group = if header == "Desktop Entry" {
                Group::Main
            } else if let Some(name) = header.strip_prefix("Desktop Action ") {
                entry.actions.push(Action {
                    id: name.to_string(),
                    name: name.to_string(),
                    exec: String::new(),
                });
                Group::Action(entry.actions.len() - 1)
            } else {
                Group::Other
            };
            continue;
        }

        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        // Ignore localised variants like `Name[de]`; the C locale is enough
        // for matching, and picking a locale is the shell's job.
        if key.contains('[') {
            continue;
        }

        match group {
            Group::Main => match key {
                "Name" => entry.name = value.to_string(),
                "Icon" => entry.icon = value.to_string(),
                "Exec" => entry.exec = value.to_string(),
                "StartupWMClass" => entry.wm_class = sane_wm_class(value),
                "NoDisplay" | "Hidden" => entry.no_display |= value == "true",
                "Terminal" => entry.terminal = value == "true",
                "Actions" => {
                    action_ids =
                        value.split(';').filter(|s| !s.is_empty()).map(str::to_owned).collect()
                }
                _ => {}
            },
            Group::Action(i) => match key {
                "Name" => entry.actions[i].name = value.to_string(),
                "Exec" => entry.actions[i].exec = value.to_string(),
                _ => {},
            },
            Group::Other => {}
        }
    }

    // Keep only actions the entry actually advertises, in declared order.
    if !action_ids.is_empty() {
        entry.actions.sort_by_key(|a| {
            action_ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX)
        });
        entry.actions.retain(|a| action_ids.contains(&a.id));
    }

    (!entry.name.is_empty()).then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsubstituted_startupwmclass_templates() {
        // Real value from Arch's chromium.desktop.
        assert_eq!(sane_wm_class("@@startup_wm_class"), None);
        assert_eq!(sane_wm_class("${WMCLASS}"), None);
        assert_eq!(sane_wm_class(""), None);
        assert_eq!(sane_wm_class("Code").as_deref(), Some("Code"));
    }

    #[test]
    fn strips_field_codes_but_keeps_literal_percent() {
        assert_eq!(strip_field_codes("chromium --app=x %U"), "chromium --app=x");
        assert_eq!(strip_field_codes("foo %F --bar %i"), "foo  --bar");
        assert_eq!(strip_field_codes("wine 100%% --go"), "wine 100% --go");
    }
}
