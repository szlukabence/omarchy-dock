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

/// Whether an `Exec=` line takes files or URLs, i.e. whether files dropped on
/// the app can be opened with it.
pub fn accepts_files(exec: &str) -> bool {
    file_code(exec).is_some()
}

/// The file field code an `Exec=` line uses: `F`, `U`, `f` or `u`.
fn file_code(exec: &str) -> Option<char> {
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some(code @ ('f' | 'F' | 'u' | 'U')) => return Some(code),
                // `%%` is a literal percent sign, not a code.
                _ => continue,
            }
        }
    }
    None
}

/// Single-quote a value for a shell command line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command lines that open `files` — each a `(path, uri)` pair — with the
/// app whose `Exec=` line is `exec`.
///
/// Follows the desktop-entry specification: `%F` and `%U` take every file in
/// one launch; `%f` and `%u` take one, so several files mean several launches.
/// Every other field code is dropped, and `%%` becomes `%`. Paths are
/// single-quoted, because the result runs through a shell and file names
/// contain spaces, quotes and worse. Empty when the app takes no files.
pub fn open_command(exec: &str, files: &[(String, String)]) -> Vec<String> {
    let Some(code) = file_code(exec) else { return Vec::new() };
    if files.is_empty() {
        return Vec::new();
    }

    // Keep only the chosen code, as a placeholder that cannot occur in the
    // quoted paths spliced in afterwards.
    const SLOT: &str = "\u{0}";
    let mut template = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            template.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => template.push('%'),
            Some(x) if x == code => template.push_str(SLOT),
            _ => {}
        }
    }

    let arg = |(path, uri): &(String, String)| {
        shell_quote(if code.eq_ignore_ascii_case(&'u') { uri } else { path })
    };
    // Tidy the spaces the dropped codes leave behind before the paths go in,
    // never after: a file name's own spaces are part of the name.
    let template = template.split_whitespace().collect::<Vec<_>>().join(" ");

    if code.is_uppercase() {
        let all: Vec<String> = files.iter().map(arg).collect();
        vec![template.replacen(SLOT, &all.join(" "), 1)]
    } else {
        files.iter().map(|f| template.replacen(SLOT, &arg(f), 1)).collect()
    }
}

/// XDG data directories, in precedence order.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::data_dir() {
        dirs.push(home);
    }
    let xdg = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in xdg.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// Directories searched for desktop entries, in precedence order.
pub fn search_dirs() -> Vec<PathBuf> {
    data_dirs().into_iter().map(|d| d.join("applications")).collect()
}

/// An icon file named `name`, found by looking in the icon folders directly.
///
/// The fallback for when the icon theme does not know the name. That happens
/// when an app installs its icon at a size the theme does not list: AionUi
/// puts its only one in `hicolor/1024x1024`, which GTK never looks in because
/// hicolor stops at 512 — yet the launcher, searching by file name, finds it,
/// so the dock drew a generic icon where the menu drew the right one.
///
/// Prefers a scalable icon, then the largest bitmap, then `pixmaps/`.
pub fn icon_file(name: &str) -> Option<PathBuf> {
    icon_file_in(&data_dirs(), name)
}

fn icon_file_in(roots: &[PathBuf], name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    for root in roots {
        let mut best: Option<(u32, PathBuf)> = None;
        if let Ok(sizes) = std::fs::read_dir(root.join("icons/hicolor")) {
            for size in sizes.flatten() {
                let dir = size.file_name().to_string_lossy().into_owned();
                // "scalable" beats any bitmap; "48x48" and "48x48@2" rank 48.
                let rank = if dir == "scalable" {
                    u32::MAX
                } else {
                    dir.split('x').next().and_then(|n| n.parse().ok()).unwrap_or(0)
                };
                for ext in ["svg", "png"] {
                    let path = size.path().join("apps").join(format!("{name}.{ext}"));
                    if path.is_file() && best.as_ref().is_none_or(|(r, _)| rank > *r) {
                        best = Some((rank, path));
                    }
                }
            }
        }
        if let Some((_, path)) = best {
            return Some(path);
        }
        for ext in ["svg", "png", "xpm"] {
            let path = root.join("pixmaps").join(format!("{name}.{ext}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
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
    /// A throwaway data dir holding `files`, relative paths under it.
    fn data_dir(tag: &str, files: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join(format!("omarchy-dock-icons-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        root
    }

    #[test]
    fn an_icon_at_a_size_the_theme_does_not_list_is_still_found() {
        // AionUi's layout: its only icon, at a size hicolor does not index.
        let root = data_dir("aion", &["icons/hicolor/1024x1024/apps/AionUi.png"]);
        assert_eq!(
            super::icon_file_in(std::slice::from_ref(&root), "AionUi"),
            Some(root.join("icons/hicolor/1024x1024/apps/AionUi.png"))
        );
        assert_eq!(super::icon_file_in(&[root], "Other"), None);
    }

    #[test]
    fn scalable_beats_the_largest_bitmap_which_beats_smaller_ones() {
        let root = data_dir("rank", &[
            "icons/hicolor/48x48/apps/app.png",
            "icons/hicolor/256x256/apps/app.png",
            "icons/hicolor/64x64@2/apps/app.png",
        ]);
        assert_eq!(
            super::icon_file_in(std::slice::from_ref(&root), "app"),
            Some(root.join("icons/hicolor/256x256/apps/app.png"))
        );
        let root = data_dir("svg", &[
            "icons/hicolor/256x256/apps/app.png",
            "icons/hicolor/scalable/apps/app.svg",
        ]);
        assert_eq!(
            super::icon_file_in(std::slice::from_ref(&root), "app"),
            Some(root.join("icons/hicolor/scalable/apps/app.svg"))
        );
    }

    #[test]
    fn pixmaps_are_the_last_resort_and_earlier_dirs_win() {
        let user = data_dir("user", &["pixmaps/app.png"]);
        let system = data_dir("system", &["icons/hicolor/128x128/apps/app.png"]);
        // The user's data dir comes first, even with only a pixmap.
        assert_eq!(
            super::icon_file_in(&[user.clone(), system], "app"),
            Some(user.join("pixmaps/app.png"))
        );
        // Names are file names, never paths.
        assert_eq!(super::icon_file_in(&[user], "../pixmaps/app"), None);
    }

    fn f(path: &str) -> (String, String) {
        (path.to_string(), format!("file://{}", path.replace(' ', "%20")))
    }

    #[test]
    fn list_codes_take_every_file_in_one_launch() {
        let cmds = open_command("code %F", &[f("/a.rs"), f("/b.rs")]);
        assert_eq!(cmds, ["code '/a.rs' '/b.rs'"]);
        let cmds = open_command("firefox %U", &[f("/x y.html")]);
        assert_eq!(cmds, ["firefox 'file:///x%20y.html'"]);
    }

    #[test]
    fn single_codes_launch_once_per_file() {
        let cmds = open_command("imv %f", &[f("/1.png"), f("/2.png")]);
        assert_eq!(cmds, ["imv '/1.png'", "imv '/2.png'"]);
    }

    #[test]
    fn spaces_inside_a_file_name_are_kept() {
        let cmds = open_command("app  %f  --x", &[f("/a  b\tc.txt")]);
        assert_eq!(cmds, ["app '/a  b\tc.txt' --x"]);
    }

    #[test]
    fn paths_are_quoted_against_the_shell() {
        let cmds = open_command("app %f", &[f("/it's $(rm -rf ~).txt")]);
        assert_eq!(cmds, [r"app '/it'\''s $(rm -rf ~).txt'"]);
    }

    #[test]
    fn other_codes_are_dropped_and_percent_percent_survives() {
        let cmds = open_command("app --name %c %i --pct=100%% %F %k", &[f("/a")]);
        assert_eq!(cmds, ["app --name --pct=100% '/a'"]);
    }

    #[test]
    fn an_app_that_takes_no_files_refuses_the_drop() {
        assert!(!accepts_files("omarchy-launch-webapp \"https://x.com\""));
        assert!(!accepts_files("app --literal=%%F"));
        assert!(open_command("spotify", &[f("/a")]).is_empty());
        assert!(accepts_files("nautilus --new-window %U"));
    }

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
