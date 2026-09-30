//! The dock's window-state engine.
//!
//! Reconciles three inputs — the pinned list, the live Hyprland window set,
//! and which window is focused — into the ordered items the UI renders.
//!
//! Deliberately GTK-free so it can be unit-tested headless. It runs on the
//! main thread anyway: owning it there means the render path takes no locks,
//! and a mutex between state and widgets is exactly what costs frames.

// `scratchpad`, `matcher()` and `add_client()` are consumed by the scratchpad
// pills and click handling in Phase 5.
#![allow(dead_code)]

pub mod drive;
pub mod matcher;

use std::collections::HashMap;

use crate::desktop::Entry;
use crate::hypr::model::{Client, Monitor, Workspace};
use crate::hypr::Address;
use matcher::Matcher;

/// Token in the pinned list that renders as a divider.
pub const SEPARATOR: &str = "---";

/// What a dock slot is. Slots are not interchangeable: a separator is narrow
/// and inert, the launcher never represents a window, and only apps can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// Omarchy menu button, pinned to the head of the dock.
    Launcher,
    App,
    /// A divider — either user-placed or the automatic one that fences pinned
    /// apps off from running-but-unpinned ones.
    Separator,
    /// A macOS-style stack: a directory whose recent contents fan out.
    Folder,
    Trash,
    /// A plugged-in removable drive. Opens in the file manager, mounting it
    /// first if need be.
    Drive,
    /// A system-tray item, hosted over D-Bus rather than owned by the dock.
    Tray,
    /// One Hyprland workspace, rendered as a numbered tile the way the bar's
    /// workspace widget does. Clicking switches to it; dropping an app icon on
    /// it sends that window there.
    Workspace,
    /// Omarchy's scratchpad, the `special:scratchpad` workspace.
    Scratchpad,
    /// A pinned shell command with a glyph, rather than an application.
    Command,
    /// Shown only while Omarchy is screen-recording: click to stop.
    Recording,
}

/// What the dock shows about one window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowMeta {
    pub title: String,
    /// Workspace name as Hyprland reports it: "3", or "special:scratchpad".
    pub workspace: String,
    /// Parked on `special:minimized` by the dock.
    pub minimized: bool,
    /// Where a minimized window goes back to, from its tag.
    pub home: Option<String>,
    /// Hyprland's focus history: lower was focused more recently.
    pub recency: i32,
    /// The other windows in its tab group, which move with it.
    pub group: Vec<Address>,
}

impl WindowMeta {
    pub fn of(c: &Client) -> Self {
        Self {
            title: c.title.clone(),
            workspace: c.workspace.name.clone(),
            minimized: c.is_minimized(),
            home: crate::hypr::minimize::home_of(&c.tags),
            recency: c.focus_history_id,
            group: c.tab_mates(),
        }
    }

    /// Short human label for the workspace: "3", or "scratchpad".
    pub fn workspace_label(&self) -> &str {
        self.workspace.strip_prefix("special:").unwrap_or(&self.workspace)
    }
}

/// One rendered dock item.
#[derive(Debug, Clone)]
pub struct DockItem {
    pub kind: ItemKind,
    /// Stable identity: desktop id for known apps, else the window class.
    pub key: String,
    pub label: String,
    pub icon: String,
    /// Windows belonging to this item, in Hyprland's order.
    pub windows: Vec<Address>,
    pub pinned: bool,
    /// True when one of this item's windows holds focus.
    pub active: bool,
    /// True when any of its windows asked for attention.
    pub urgent: bool,
    /// True when every window is on a special (scratchpad) workspace.
    pub scratchpad: bool,
    /// Which of `windows` currently holds focus, for click-to-cycle.
    pub active_window: Option<Address>,
    /// Command line to launch when nothing is running.
    pub exec: String,
    /// `Desktop Action` entries, offered in the context menu.
    pub actions: Vec<crate::desktop::Action>,
    /// Filesystem path, for folder stacks and Trash.
    pub path: Option<std::path::PathBuf>,
    /// Index in `items.pinned`, for entries that live there. `None` for
    /// derived items — running-but-unpinned apps, automatic dividers, the
    /// launcher, folders, drives and Trash — which have nothing to reorder.
    pub pin_index: Option<usize>,
    /// Monochrome glyph to draw instead of a themed icon, when the dock's
    /// furniture is rendered the way the bar's widgets are. `None` for real
    /// applications, which always keep their own icon.
    pub glyph: Option<String>,
    /// Raw icon pixels a tray item supplied itself, for the many applications
    /// that ship no themed icon. Shared, because the item list is cloned on
    /// every rebuild.
    pub pixmap: Option<std::sync::Arc<(i32, i32, Vec<u8>)>>,
    /// Title and workspace of each window in `windows`, index for index. The
    /// window list in the context menu and the hover previews both need to say
    /// *which* window is which — with an app open on several workspaces,
    /// "Window 1 / Window 2" is no help at all.
    pub window_meta: Vec<WindowMeta>,
    /// The raw `Exec=` line, when the app declares that it opens files or URLs
    /// (`%f`, `%F`, `%u`, `%U`). `None` means a file dropped on it is refused
    /// rather than launching the app and quietly ignoring the file.
    pub open_with: Option<String>,
    /// The media player belonging to this app, while one exists.
    pub media: Option<crate::media::Player>,
    /// Notifications this app has sent since one of its windows last had
    /// focus. Zero when badges are off.
    pub unread: usize,
    /// For the Downloads stack, how many downloads are still in progress.
    pub downloading: usize,
}

/// What a left-click on an app does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    /// Nothing running: launch it.
    Launch,
    Focus(Address),
    Minimize(Address),
    Restore(Address),
}

impl DockItem {
    /// Whether this item gets a running-indicator dot beneath it.
    ///
    /// A workspace tile already says how full it is through its own brightness
    /// and fill, so a dot underneath repeats it. Command tiles never run
    /// anything.
    pub fn shows_indicator(&self) -> bool {
        !matches!(self.kind, ItemKind::Workspace | ItemKind::Command) && self.running()
    }

    pub fn running(&self) -> bool {
        !self.windows.is_empty()
    }

    /// Separators take no input and show no indicator.
    pub fn interactive(&self) -> bool {
        self.kind != ItemKind::Separator
    }

    /// The name `omarchy-webapp-remove` knows this app by, if it is an Omarchy
    /// web app: one whose launcher, in the user's applications folder, runs
    /// Omarchy's web-app wrapper. That is the test the removal script applies
    /// itself, and its name for the app is the launcher's file name.
    pub fn omarchy_webapp(&self) -> Option<String> {
        self.omarchy_webapp_in(&dirs::data_dir()?.join("applications"))
    }

    fn omarchy_webapp_in(&self, dir: &std::path::Path) -> Option<String> {
        let wrapped = self.exec.contains("omarchy-launch-webapp")
            || self.exec.contains("omarchy-webapp-handler");
        if self.kind != ItemKind::App || !wrapped || self.key.contains('/') {
            return None;
        }
        dir.join(format!("{}.desktop", self.key)).is_file().then(|| self.key.clone())
    }

    /// The command that removes this web app, as Omarchy's menu would.
    pub fn remove_webapp_command(&self) -> Option<String> {
        self.omarchy_webapp()
            .map(|name| format!("omarchy-webapp-remove {}", matcher::shell_quote(&name)))
    }

    /// What a middle-click runs: another window of this app, even when one is
    /// already open.
    ///
    /// The entry's own "New Window" action when it has one (Chromium, Edge,
    /// VS Code's "New Empty Window"), since launching some apps again only
    /// raises the window they have. Otherwise the launch command — except for
    /// a web app pinned by its window class, whose command is "launch or
    /// focus" and so must be swapped for a plain launch.
    pub fn new_window_command(&self) -> Option<String> {
        if self.kind != ItemKind::App {
            return None;
        }
        let action = self.actions.iter().find(|a| {
            let id = a.id.to_ascii_lowercase().replace(['-', '_'], "");
            id == "newwindow" || id == "newemptywindow"
        });
        if let Some(exec) = action.map(|a| crate::desktop::strip_field_codes(&a.exec)) {
            if !exec.is_empty() {
                return Some(exec);
            }
        }
        if self.exec.contains("launch or focus webapp") {
            if let Some((_, url)) = matcher::webapp_from_class(&self.key) {
                return Some(format!("omarchy-launch-webapp {}", matcher::shell_quote(&url)));
            }
        }
        (!self.exec.is_empty()).then(|| self.exec.clone())
    }

    /// The number on the icon: unread notifications when there are any —
    /// they are what a badge is for — otherwise how many windows are open.
    pub fn badge(&self) -> Option<usize> {
        if self.unread > 0 {
            return Some(self.unread);
        }
        if self.downloading > 0 {
            return Some(self.downloading);
        }
        (self.windows.len() > 1).then_some(self.windows.len())
    }

    /// What the dock knows about one of this item's windows.
    pub fn meta_of(&self, addr: &Address) -> Option<&WindowMeta> {
        let at = self.windows.iter().position(|w| w == addr)?;
        self.window_meta.get(at)
    }

    fn is_minimized(&self, addr: &Address) -> bool {
        self.meta_of(addr).is_some_and(|m| m.minimized)
    }

    /// Running, with every window minimized: its dot dims.
    pub fn all_minimized(&self) -> bool {
        !self.windows.is_empty() && self.windows.iter().all(|w| self.is_minimized(w))
    }

    /// What a left-click does.
    ///
    /// Clicking the app in front minimizes its focused window; picking one of
    /// several is what the hover previews are for. Clicking from elsewhere
    /// goes to its first window on screen. An app with every window minimized
    /// gets back the one minimized last — the most recently focused of them.
    pub fn click(&self) -> Click {
        let focused = self
            .active_window
            .as_ref()
            .filter(|a| self.windows.contains(a) && !self.is_minimized(a));
        if let Some(a) = focused {
            return Click::Minimize(a.clone());
        }
        if let Some(a) = self.windows.iter().find(|w| !self.is_minimized(w)) {
            return Click::Focus(a.clone());
        }
        self.last_minimized().map_or(Click::Launch, |a| Click::Restore(a.clone()))
    }

    /// Which window a drop on a workspace tile sends: the focused one, else
    /// the first on screen, and only when every window is minimized, the one
    /// minimized last.
    pub fn drop_target(&self) -> Option<&Address> {
        let out = |w: &&Address| !self.is_minimized(w);
        self.active_window
            .as_ref()
            .filter(|a| self.windows.contains(a))
            .filter(out)
            .or_else(|| self.windows.iter().find(out))
            .or_else(|| self.last_minimized())
    }

    /// Of this item's windows, the most recently focused — which, among
    /// minimized ones, is the one minimized last.
    fn last_minimized(&self) -> Option<&Address> {
        self.windows
            .iter()
            .filter(|w| self.is_minimized(w))
            .min_by_key(|w| self.meta_of(w).map_or(i32::MAX, |m| m.recency))
    }
}

/// Key prefix for a tray item. The D-Bus service follows, which is both the
/// item's identity and the address every click has to be sent to.
const TRAY_KEY: &str = "__tray:";

/// D-Bus service a tray item refers to, if it is one.
pub fn tray_service(key: &str) -> Option<&str> {
    key.strip_prefix(TRAY_KEY)
}

/// Key prefix for a removable drive. The drive's id follows, which is what
/// opening and ejecting look it up by.
const DRIVE_KEY: &str = "__drive:";

/// Id of the drive a dock item stands for, if it is one.
pub fn drive_of(key: &str) -> Option<&str> {
    key.strip_prefix(DRIVE_KEY)
}

/// Fallback glyph for a tray item that ships neither a themed icon nor a
/// pixmap. Rare, but an invisible dock item is worse than a generic one.
const GLYPH_TRAY: &str = "\u{f013}";

fn tray_item(t: &crate::tray::TrayItem) -> DockItem {
    DockItem {
        kind: ItemKind::Tray,
        key: format!("{TRAY_KEY}{}", t.service),
        label: t.label().to_string(),
        icon: t.icon_name.clone(),
        windows: Vec::new(),
        pinned: false,
        active: false,
        urgent: t.needs_attention(),
        scratchpad: false,
        active_window: None,
        exec: String::new(),
        actions: Vec::new(),
        path: None,
        pin_index: None,
        // Only when there is nothing else to draw: a themed name or the app's
        // own pixmap both beat a generic mark.
        glyph: (t.icon_name.is_empty() && t.pixmap.is_none())
            .then(|| GLYPH_TRAY.to_string()),
        pixmap: t.pixmap.clone(),
        window_meta: Vec::new(),
        open_with: None,
        media: None,
        unread: 0,
        downloading: 0,
    }
}

/// Prefix marking a pinned entry as a command tile rather than an app id.
pub const COMMAND_KEY: &str = "cmd:";

/// Generic mark for a command tile whose config gives no glyph.
const GLYPH_COMMAND: &str = "\u{f120}";

/// The bar's own screen-recording glyph, so the two read as one thing.
const GLYPH_RECORDING: &str = "\u{f0ec2}";

/// Omarchy's command for stopping a recording — what the bar's indicator runs.
const STOP_RECORDING: &str = "omarchy-capture-screenrecording --stop-recording";

/// The window class `omarchy-launch-screensaver` gives its terminals.
const SCREENSAVER_CLASS: &str = "org.omarchy.screensaver";

/// A pinned shell command, as a dock item.
fn command_item(cmd: &crate::config::CommandItem, pin_index: usize) -> DockItem {
    DockItem {
        kind: ItemKind::Command,
        key: format!("{COMMAND_KEY}{}", cmd.id),
        label: cmd.label.clone(),
        icon: String::new(),
        windows: Vec::new(),
        pinned: true,
        active: false,
        urgent: false,
        scratchpad: false,
        active_window: None,
        exec: cmd.command.clone(),
        actions: Vec::new(),
        path: None,
        pin_index: Some(pin_index),
        glyph: Some(if cmd.glyph.is_empty() {
            GLYPH_COMMAND.to_string()
        } else {
            cmd.glyph.clone()
        }),
        pixmap: None,
        window_meta: Vec::new(),
        open_with: None,
        media: None,
        unread: 0,
        downloading: 0,
    }
}

/// Key prefix for a workspace tile./// Key prefix for a workspace tile. The workspace's own name follows, so the
/// key is stable across updates — workspace 3 is always workspace 3.
const WORKSPACE_KEY: &str = "__ws:";

/// Omarchy names its scratchpad `special:scratchpad`, and binds SUPER+S to it.
pub const SCRATCHPAD: &str = "scratchpad";

/// Workspace name a workspace tile refers to, if it is one.
pub fn workspace_of(key: &str) -> Option<&str> {
    key.strip_prefix(WORKSPACE_KEY)
}

/// The Omarchy logo, from the `omarchy` font at `/usr/share/fonts/omarchy/`.
///
/// The same codepoint the shell's own menu bar widget draws, so the dock's
/// launcher button is literally the same mark as the one in the bar.
pub const GLYPH_LAUNCHER: &str = "\u{e900}";

/// Nerd Font glyphs for the dock's folder stacks and Trash. Chosen to match
/// the vocabulary the Omarchy menu uses for the same concepts.
const GLYPH_FOLDER: &str = "\u{f07b}";
const GLYPH_HOME: &str = "\u{f015}";
const GLYPH_DOWNLOADS: &str = "\u{f019}";
const GLYPH_DOCUMENTS: &str = "\u{f0f6}";
const GLYPH_PICTURES: &str = "\u{f03e}";
const GLYPH_TRASH: &str = "\u{f1f8}";
const GLYPH_TRASH_FULL: &str = "\u{f014}";
/// Scratchpad: a drawer to put windows in.
const GLYPH_SCRATCHPAD: &str = "\u{f01c}";

/// The glyph that stands for a directory, by what the directory *is*.
///
/// Matched on the XDG user-directory basename rather than the label, so a
/// renamed stack keeps the right mark.
pub fn folder_glyph(path: &std::path::Path) -> &'static str {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match name.as_str() {
        "downloads" => GLYPH_DOWNLOADS,
        "documents" => GLYPH_DOCUMENTS,
        "pictures" | "photos" => GLYPH_PICTURES,
        _ => {
            // The home directory has no distinguishing basename, so compare
            // the path itself.
            if dirs::home_dir().is_some_and(|h| h == path) {
                GLYPH_HOME
            } else {
                GLYPH_FOLDER
            }
        }
    }
}

/// The `Exec=` line to open dropped files with, if the entry takes files.
///
/// Terminal apps are excluded: their command needs a terminal around it, and
/// launching it bare with a file would fail invisibly.
fn open_with(entry: Option<&Entry>) -> Option<String> {
    entry
        .filter(|e| !e.terminal && crate::desktop::accepts_files(&e.exec))
        .map(|e| e.exec.clone())
}

/// The `n`th (0-based) item a "dock app N" keybinding should reach.
///
/// Counts applications and pinned command tiles in the order they appear, and
/// skips the dock's furniture — the launcher, dividers, workspace tiles, the
/// scratchpad, folder stacks and Trash. Counting every slot made `activate 1`
/// open the Omarchy menu, and shifted the numbering whenever a divider was
/// added.
pub fn nth_app(items: &[DockItem], n: usize) -> Option<&DockItem> {
    items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::App | ItemKind::Command))
        .nth(n)
}

/// Identity used to match this item against an updated item list.
///
/// Normally the key, but separators are deliberately interchangeable: they
/// render identically and hold no state, so any user-placed divider may stand
/// in for any other. Their key encodes their position — which changes the
/// moment one is dragged — so matching them on it would make an in-place
/// reorder impossible and force a full rebuild every time.
///
/// User separators and automatic dividers still form two distinct classes:
/// only the former can be moved or removed.
pub fn match_key(item: &DockItem) -> &str {
    if item.kind != ItemKind::Separator {
        return &item.key;
    }
    if item.pin_index.is_some() {
        "\u{0}separator:user"
    } else {
        "\u{0}separator:auto"
    }
}

/// For each position in `new`, which position in `old` holds that same item.
///
/// `None` when the two lists are not a permutation of each other, which means
/// the item *set* changed and the caller needs new widgets rather than a
/// rearrangement of the ones it has.
///
/// Each old entry may be claimed only once. That matters because separators
/// deliberately share a match key: without it every divider would map to the
/// first one and the rest of the mapping would be nonsense.
pub fn match_permutation(old: &[DockItem], new: &[DockItem]) -> Option<Vec<usize>> {
    if old.len() != new.len() {
        return None;
    }
    let mut taken = vec![false; old.len()];
    let mut from = Vec::with_capacity(new.len());
    for item in new {
        let want = match_key(item);
        let at = old
            .iter()
            .enumerate()
            .position(|(i, d)| !taken[i] && match_key(d) == want)?;
        taken[at] = true;
        from.push(at);
    }
    Some(from)
}

/// A separator the user placed at `pin_index` in the pinned list.
fn user_separator(pin_index: usize) -> DockItem {
    let mut item = separator();
    item.key = format!("{SEPARATOR}:{pin_index}");
    item.pin_index = Some(pin_index);
    item
}

/// Shift `index` by `delta` places within `list`.
///
/// Clamps at the ends rather than wrapping: an item that jumped from one end of
/// the dock to the other would be surprising, and the menu gives no hint that
/// it might.
pub fn move_in_list<T>(list: &mut [T], index: usize, delta: i32) -> bool {
    if index >= list.len() || list.is_empty() {
        return false;
    }
    let target = (index as i32 + delta).clamp(0, list.len() as i32 - 1) as usize;
    if target == index {
        return false;
    }
    list.swap(index, target);
    true
}

/// Move `from` to `to` within `list`, shifting the rest.
///
/// Insert semantics, not a swap: dragging an icon between two others should
/// land it there, not exchange it with whatever it was dropped on.
pub fn reorder_in_list<T>(list: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from >= list.len() || to > list.len() || from == to {
        return false;
    }
    let item = list.remove(from);
    // Removing shifts everything after `from` down by one.
    let to = if to > from { to - 1 } else { to };
    list.insert(to.min(list.len()), item);
    true
}

/// Index in the pinned list that a separator item refers to, if it is one the
/// user placed rather than an automatic divider.
pub fn separator_pin_index(key: &str) -> Option<usize> {
    key.strip_prefix(SEPARATOR)?.strip_prefix(':')?.parse().ok()
}

fn separator() -> DockItem {
    DockItem {
        kind: ItemKind::Separator,
        key: SEPARATOR.into(),
        label: String::new(),
        icon: String::new(),
        windows: Vec::new(),
        pinned: false,
        active: false,
        urgent: false,
        scratchpad: false,
        active_window: None,
        exec: String::new(),
        actions: Vec::new(),
        path: None,
        pin_index: None,
        glyph: None,
        pixmap: None,
        window_meta: Vec::new(),
        open_with: None,
        media: None,
        unread: 0,
        downloading: 0,
    }
}

pub struct DockState {
    matcher: Matcher,
    clients: Vec<Client>,
    monitors: Vec<Monitor>,
    workspaces: Vec<Workspace>,
    tray: Vec<crate::tray::TrayItem>,
    drives: Vec<drive::Drive>,
    media: Vec<crate::media::Player>,
    focused: Option<Address>,
    urgent: Vec<Address>,
    /// A minimized window already sent home by `recall`, so it is not sent
    /// twice while the snapshot showing it gone is still on its way.
    recalled: Option<Address>,
    /// Unread notification counts by item key.
    unread: HashMap<String, usize>,
    /// Whether Omarchy is screen-recording.
    recording: bool,
    /// Downloads in progress in the Downloads folder.
    downloads: usize,
}

impl DockState {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            matcher: Matcher::build(entries),
            clients: Vec::new(),
            monitors: Vec::new(),
            workspaces: Vec::new(),
            tray: Vec::new(),
            drives: Vec::new(),
            media: Vec::new(),
            focused: None,
            urgent: Vec::new(),
            recalled: None,
            unread: HashMap::new(),
            recording: false,
            downloads: 0,
        }
    }

    pub fn downloads(&self) -> usize {
        self.downloads
    }

    pub fn set_downloads(&mut self, n: usize) {
        self.downloads = n;
    }

    pub fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    /// Count a notification for the app it belongs to, unless that app is
    /// the one in front, where it has been seen as it arrived. Returns
    /// whether anything changed.
    pub fn note_notice(&mut self, items: &[DockItem], notice: &crate::notices::Notice) -> bool {
        let Some(item) = notice_target(items, notice) else { return false };
        if item.active {
            return false;
        }
        *self.unread.entry(item.key.clone()).or_default() += 1;
        true
    }

    /// Clear the count of every app with a focused window: looking at it is
    /// what reading means here. Returns whether anything changed.
    pub fn clear_seen(&mut self, items: &[DockItem]) -> bool {
        let before = self.unread.len();
        self.unread
            .retain(|key, _| !items.iter().any(|i| i.active && &i.key == key));
        self.unread.len() != before
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    /// Replace the window set wholesale, as after a snapshot or reconnect.
    /// Replace the known desktop entries after an application was installed or
    /// removed.
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.matcher = Matcher::build(entries);
    }

    pub fn set_clients(&mut self, clients: Vec<Client>) {
        self.clients = clients;
        // An urgent window that has since closed must not stay urgent.
        self.urgent.retain(|a| self.clients.iter().any(|c| &c.address == a));
        // A recalled window that is no longer parked has made its trip.
        if let Some(a) = &self.recalled {
            if !self.clients.iter().any(|c| &c.address == a && c.is_minimized()) {
                self.recalled = None;
            }
        }
    }

    pub fn set_monitors(&mut self, monitors: Vec<Monitor>) {
        self.monitors = monitors;
    }

    pub fn set_media(&mut self, players: Vec<crate::media::Player>) {
        self.media = players;
    }

    pub fn set_tray(&mut self, items: Vec<crate::tray::TrayItem>) {
        self.tray = items;
    }

    pub fn set_drives(&mut self, drives: Vec<drive::Drive>) {
        self.drives = drives;
    }

    pub fn set_workspaces(&mut self, mut workspaces: Vec<Workspace>) {
        // Hyprland reports them in creation order, which would make tiles jump
        // around as workspaces come and go.
        workspaces.sort_by_key(|w| w.id);
        self.workspaces = workspaces;
    }

    /// Whether any window on a currently visible workspace is fullscreen.
    ///
    /// Restricted to visible workspaces: a fullscreen window parked on another
    /// workspace is not covering anything here, and hiding for it would leave
    /// the dock gone with nothing on screen to explain why.
    pub fn has_fullscreen(&self) -> bool {
        let visible: Vec<i32> =
            self.monitors.iter().map(|m| m.active_workspace.id).collect();
        self.clients
            .iter()
            .any(|c| c.fullscreen > 0 && visible.contains(&c.workspace.id))
    }

    /// Whether Omarchy's screensaver is up.
    ///
    /// It is an ordinary fullscreen terminal, so `has_fullscreen` already
    /// hides the dock for it — but a hidden dock still peeks when the pointer
    /// reaches the edge, and over a screensaver nothing should appear.
    pub fn screensaver_showing(&self) -> bool {
        self.clients.iter().any(|c| c.class == SCREENSAVER_CLASS)
    }

    /// The workspace currently shown on the focused monitor, or the first.
    fn active_workspace_id(&self) -> Option<i32> {
        let monitor = self.monitors.iter().find(|m| m.focused).or(self.monitors.first())?;
        Some(monitor.active_workspace.id)
    }

    /// Whether a special workspace is open on any monitor.
    fn special_is_open(&self) -> bool {
        self.monitors.iter().any(|m| {
            m.special_workspace.as_ref().is_some_and(|w| !w.name.is_empty())
        })
    }

    pub fn monitor_by_name(&self, name: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.name == name)
    }

    /// Record that focus moved to monitor `name`, ahead of the next snapshot.
    pub fn set_focused_monitor(&mut self, name: &str) {
        for m in &mut self.monitors {
            m.focused = m.name == name;
        }
    }

    /// The monitor Hyprland currently considers focused.
    pub fn focused_monitor(&self) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.focused)
    }

    /// The focused window, if the dock knows about it.
    pub fn focused_client(&self) -> Option<&Client> {
        let addr = self.focused.as_ref()?;
        self.clients.iter().find(|c| &c.address == addr)
    }

    /// A minimized window that something outside the dock just focused — a
    /// launch-or-focus key, a notification, an app activating itself.
    /// Hyprland answers by opening all of `special:minimized` over the
    /// screen; what the focus meant was "bring it back", so the caller
    /// restores it. Returned once per trip.
    pub fn recall(&mut self) -> Option<Client> {
        let c = self.focused_client().filter(|c| c.is_minimized())?.clone();
        if self.recalled.as_ref() == Some(&c.address) {
            return None;
        }
        self.recalled = Some(c.address.clone());
        Some(c)
    }

    /// The window minimized last — the most recently focused of the
    /// minimized ones, since it had focus when it went.
    pub fn last_minimized(&self) -> Option<&Client> {
        self.clients.iter().filter(|c| c.is_minimized()).min_by_key(|c| c.focus_history_id)
    }

    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    pub fn add_client(&mut self, client: Client) {
        self.clients.retain(|c| c.address != client.address);
        self.clients.push(client);
    }

    pub fn remove_client(&mut self, addr: &Address) {
        self.clients.retain(|c| &c.address != addr);
        self.urgent.retain(|a| a != addr);
        if self.focused.as_ref() == Some(addr) {
            self.focused = None;
        }
    }

    pub fn set_focused(&mut self, addr: Option<Address>) {
        // Focusing a window clears its attention request, as in macOS.
        if let Some(a) = &addr {
            self.urgent.retain(|u| u != a);
        }
        self.focused = addr;
    }

    /// Record an attention request. Returns whether it counts — a window in
    /// front has the user's attention already — so the caller knows whether
    /// to pulse. A repeat request counts again.
    pub fn set_urgent(&mut self, addr: Address) -> bool {
        if self.focused.as_ref() == Some(&addr) {
            return false;
        }
        if !self.urgent.contains(&addr) {
            self.urgent.push(addr);
        }
        true
    }

    pub fn set_title(&mut self, addr: &Address, title: String) {
        if let Some(c) = self.clients.iter_mut().find(|c| &c.address == addr) {
            c.title = title;
        }
    }

    /// Build the full ordered dock: launcher, pinned apps (with any
    /// user-placed separators), an automatic divider, running-but-unpinned
    /// apps, then Trash.
    pub fn items(&self, cfg: &crate::config::Config) -> Vec<DockItem> {
        let pinned = &cfg.items.pinned;
        let show_running = cfg.items.show_running;
        let mut items: Vec<DockItem> = Vec::new();

        if cfg.launcher.enabled {
            items.push(DockItem {
                kind: ItemKind::Launcher,
                key: "__launcher".into(),
                label: "Omarchy".into(),
                icon: cfg.launcher.icon.clone(),
                windows: Vec::new(),
                pinned: true,
                active: false,
                urgent: false,
                scratchpad: false,
                active_window: None,
                exec: cfg.launcher_command(),
                actions: Vec::new(),
                path: None,
                pin_index: None,
                glyph: Some(GLYPH_LAUNCHER.into()),
                pixmap: None,
                window_meta: Vec::new(),
                open_with: None,
                media: None,
                unread: 0,
                downloading: 0,
            });
        }

        // While recording, a stop button beside the launcher: the bar has one
        // too, but the bar may be what is being recorded around.
        if self.recording {
            items.push(DockItem {
                kind: ItemKind::Recording,
                key: "__recording".into(),
                label: "Stop recording".into(),
                icon: String::new(),
                windows: Vec::new(),
                pinned: false,
                active: false,
                urgent: false,
                scratchpad: false,
                active_window: None,
                exec: STOP_RECORDING.into(),
                actions: Vec::new(),
                path: None,
                pin_index: None,
                glyph: Some(GLYPH_RECORDING.into()),
                pixmap: None,
                window_meta: Vec::new(),
                open_with: None,
                media: None,
                unread: 0,
                downloading: 0,
            });
        }

        // Workspaces sit at the head, next to the launcher — the same place
        // the bar puts its workspace widget.
        let head_end = items.len();
        items.extend(self.workspace_items(cfg));
        if items.len() > head_end && head_end > 0 {
            items.insert(head_end, separator());
        }
        let strip_end = items.len();

        // Which clients have been claimed by a pinned slot.
        let mut claimed: Vec<bool> = vec![false; self.clients.len()];

        for (pin_index, id) in pinned.iter().enumerate() {
            if id.trim() == SEPARATOR {
                // Carry the pinned-list index so the context menu can move or
                // remove this exact separator. Automatic dividers get no index
                // and are therefore not editable, which is right: they are
                // derived, not placed.
                items.push(user_separator(pin_index));
                continue;
            }
            // A command tile: a glyph and a shell command rather than an app.
            // Looked up rather than inlined so the pinned list stays a flat
            // ordered list of ids, and dragging works the same for both.
            if let Some(id) = id.trim().strip_prefix(COMMAND_KEY) {
                if let Some(c) = cfg.items.commands.iter().find(|c| c.id == id) {
                    items.push(command_item(c, pin_index));
                } else {
                    tracing::warn!(id, "pinned command has no [[items.commands]] entry");
                }
                continue;
            }
            let entry = self.matcher.resolve_pin(id);
            let expected = self.matcher.expected_classes(id);

            let mut windows = Vec::new();
            for (i, c) in self.clients.iter().enumerate() {
                if claimed[i] {
                    continue;
                }
                let key = c.match_key();
                let matched = expected.iter().any(|e| key.eq_ignore_ascii_case(e));
                if matched {
                    claimed[i] = true;
                    windows.push(c.address.clone());
                }
            }

            // A pin can name a Chromium web-app class directly — that is what
            // pinning an ad-hoc web app stores, since it has no .desktop file.
            // Recovering the URL from the class is what keeps such a pin
            // clickable after its window closes.
            let webapp =
                entry.is_none().then(|| matcher::webapp_from_class(id)).flatten();

            let mut pinned_item = self.make_item(
                id.clone(),
                entry
                    .map(|e| e.name.clone())
                    .or_else(|| webapp.as_ref().map(|(l, _)| l.clone()))
                    .unwrap_or_else(|| id.clone()),
                entry
                    .map(|e| e.icon.clone())
                    .filter(|i| !i.is_empty())
                    .or_else(|| webapp.as_ref().map(|_| "web-browser".to_string()))
                    .unwrap_or_else(|| id.clone()),
                windows,
                true,
                entry.map(|e| e.command()).unwrap_or_else(|| {
                    webapp
                        .as_ref()
                        .map(|(_, url)| matcher::webapp_command(id, url))
                        .unwrap_or_default()
                }),
                entry.map(|e| e.actions.clone()).unwrap_or_default(),
            );
            pinned_item.pin_index = Some(pin_index);
            pinned_item.open_with = open_with(entry);
            items.push(pinned_item);
        }

        // Everything appended from here is a distinct section.
        let pinned_end = items.len();

        // Minimized windows show even when running apps don't: their icon is
        // the only way back to them.
        if show_running || self.clients.iter().any(|c| c.is_minimized()) {
            // Group leftovers by matched entry so multiple windows of one app
            // collapse into a single icon.
            type Group = (String, String, String, Vec<Address>, String, Vec<crate::desktop::Action>);
            let mut groups: Vec<Group> = Vec::new();
            for (i, c) in self.clients.iter().enumerate() {
                // Scratchpad windows show on its tile, not as apps; minimized
                // ones stay on their app's icon, the only way back to them.
                if claimed[i]
                    || (c.is_special() && !c.is_minimized())
                    || (!show_running && !c.is_minimized())
                {
                    continue;
                }
                let entry = self.matcher.match_class(c.match_key());
                let key = entry.map(|e| e.id.clone()).unwrap_or_else(|| c.match_key().to_string());
                // A Chromium web app with no .desktop file still encodes its
                // whole URL in its window class, so it can be given a real
                // Omarchy launch command rather than being pinnable but dead.
                let webapp = entry
                    .is_none()
                    .then(|| matcher::webapp_from_class(c.match_key()))
                    .flatten();
                let label = entry
                    .map(|e| e.name.clone())
                    .or_else(|| webapp.as_ref().map(|(l, _)| l.clone()))
                    .unwrap_or_else(|| c.class.clone());
                let icon = entry
                    .map(|e| e.icon.clone())
                    .filter(|i| !i.is_empty())
                    // Omarchy's own web apps use the browser's icon when they
                    // have none of their own.
                    .or_else(|| webapp.as_ref().map(|_| "web-browser".to_string()))
                    .unwrap_or_else(|| c.class.clone());
                let exec = entry.map(|e| e.command()).unwrap_or_else(|| {
                    webapp
                        .as_ref()
                        .map(|(_, url)| matcher::webapp_command(c.match_key(), url))
                        .unwrap_or_default()
                });

                match groups.iter_mut().find(|g| g.0 == key) {
                    Some(g) => g.3.push(c.address.clone()),
                    None => groups.push((
                        key,
                        label,
                        icon,
                        vec![c.address.clone()],
                        exec,
                        entry.map(|e| e.actions.clone()).unwrap_or_default(),
                    )),
                }
            }
            // Fence running-but-unpinned apps off from the pinned ones, but
            // only when there is something on both sides to divide.
            if !groups.is_empty() && pinned_end > 0 {
                items.insert(pinned_end, separator());
            }
            for (key, label, icon, windows, exec, actions) in groups {
                let mut item = self.make_item(key, label, icon, windows, false, exec, actions);
                item.open_with = open_with(self.matcher.by_id(&item.key));
                items.push(item);
            }
        }

        // Fence the strip off from the pinned apps, but only if any were
        // actually added and something follows them.
        if strip_end > head_end && items.len() > strip_end {
            items.insert(strip_end, separator());
        }

        // Stacks, drives and Trash form the dock's tail section, as on macOS.
        let tail_start = items.len();

        if cfg.tray.enabled {
            for t in &self.tray {
                if t.status == "Passive" && !cfg.tray.show_passive {
                    continue;
                }
                items.push(tray_item(t));
            }
        }

        let downloads_dir = dirs::download_dir();
        for folder in cfg.items.folders.iter().filter(|f| f.enabled) {
            let path = crate::config::expand_tilde(&folder.path);
            let downloading =
                if downloads_dir.as_ref() == Some(&path) { self.downloads } else { 0 };
            let icon = if folder.icon.is_empty() { "folder".to_string() } else { folder.icon.clone() };
            items.push(DockItem {
                kind: ItemKind::Folder,
                key: format!("__folder:{}", path.display()),
                label: folder.name.clone(),
                icon,
                windows: Vec::new(),
                pinned: true,
                active: false,
                urgent: false,
                scratchpad: false,
                active_window: None,
                exec: String::new(),
                actions: Vec::new(),
                pin_index: None,
                glyph: Some(folder_glyph(&path).into()),
                pixmap: None,
                window_meta: Vec::new(),
                open_with: None,
                media: None,
                unread: 0,
                downloading,
                path: Some(path),
            });
        }

        if cfg.items.show_drives {
            for d in &self.drives {
                items.push(DockItem {
                    kind: ItemKind::Drive,
                    key: format!("{DRIVE_KEY}{}", d.id),
                    label: d.name.clone(),
                    icon: d.icon.clone(),
                    windows: Vec::new(),
                    pinned: true,
                    active: false,
                    urgent: false,
                    scratchpad: false,
                    active_window: None,
                    exec: String::new(),
                    actions: Vec::new(),
                    path: None,
                    pin_index: None,
                    glyph: Some(d.kind.glyph().into()),
                    pixmap: None,
                    window_meta: Vec::new(),
                    open_with: None,
                    media: None,
                    unread: 0,
                    downloading: 0,
                });
            }
        }

        if cfg.items.show_trash {
            let empty = crate::stacks::trash_is_empty();
            items.push(DockItem {
                kind: ItemKind::Trash,
                key: "__trash".into(),
                label: "Trash".into(),
                // Icon switches to user-trash-full when it has contents.
                icon: if empty { "user-trash".into() } else { "user-trash-full".into() },
                windows: Vec::new(),
                pinned: true,
                active: false,
                urgent: false,
                scratchpad: false,
                active_window: None,
                exec: String::new(),
                actions: Vec::new(),
                path: Some(crate::stacks::trash_files_dir()),
                pin_index: None,
                glyph: Some(if empty { GLYPH_TRASH.into() } else { GLYPH_TRASH_FULL.into() }),
                pixmap: None,
                window_meta: Vec::new(),
                open_with: None,
                media: None,
                unread: 0,
                downloading: 0,
            });
        }

        if items.len() > tail_start && tail_start > 0 {
            items.insert(tail_start, separator());
        }

        // Two dividers in a row divide nothing between them. This happens
        // whenever a user separator sits where an automatic one is also
        // inserted, e.g. a trailing "---" meeting the folders divider.
        items.dedup_by(|a, b| a.kind == ItemKind::Separator && b.kind == ItemKind::Separator);

        // Attach each media player to the app it belongs to.
        if cfg.items.media_controls {
            for item in items.iter_mut().filter(|i| i.kind == ItemKind::App) {
                item.media = self.media.iter().find(|p| p.belongs_to(&item.key)).cloned();
            }
        }

        // A separator at either end divides nothing either.
        while items.first().is_some_and(|i| i.kind == ItemKind::Separator) {
            items.remove(0);
        }
        while items.len() > 1 && items.last().is_some_and(|i| i.kind == ItemKind::Separator) {
            items.pop();
        }

        if cfg.items.notification_badges {
            for item in items.iter_mut().filter(|i| i.kind == ItemKind::App) {
                item.unread = self.unread.get(&item.key).copied().unwrap_or(0);
            }
        }
        items
    }

    /// The workspace strip and scratchpad tile, when enabled.
    ///
    /// Windows on a workspace go into the item's `windows`, which is what the
    /// rest of the dock already keys the running indicator and the count badge
    /// off — so an occupied workspace lights up without any new plumbing.
    fn workspace_items(&self, cfg: &crate::config::Config) -> Vec<DockItem> {
        let mut items = Vec::new();
        let active = self.active_workspace_id();

        if cfg.workspaces.enabled {
            // Hyprland destroys a workspace the moment it empties, so it only
            // ever reports the ones in use. A fixed row to aim at — what the
            // bar shows, and what a drop needs — has to be filled in here.
            let mut shown: Vec<(i32, String)> = self
                .workspaces
                .iter()
                .filter(|w| !w.is_special())
                .map(|w| (w.id, w.name.clone()))
                .collect();
            if cfg.workspaces.show_empty {
                for id in 1..=cfg.workspaces.persistent.min(10) as i32 {
                    if !shown.iter().any(|(i, _)| *i == id) {
                        shown.push((id, id.to_string()));
                    }
                }
            }
            shown.sort_by_key(|(id, _)| *id);

            for (id, name) in shown {
                let windows: Vec<Address> = self
                    .clients
                    .iter()
                    .filter(|c| c.workspace.id == id)
                    .map(|c| c.address.clone())
                    .collect();

                // An empty workspace is still worth a tile when the user wants
                // a fixed row to aim at; otherwise only occupied ones and the
                // current one are shown.
                if windows.is_empty() && !cfg.workspaces.show_empty && Some(id) != active {
                    continue;
                }

                items.push(DockItem {
                    kind: ItemKind::Workspace,
                    key: format!("{WORKSPACE_KEY}{name}"),
                    // Workspace 10 sits on the 0 key, and the bar labels it so.
                    label: if id == 10 { "0".into() } else { name.clone() },
                    icon: String::new(),
                    windows,
                    pinned: false,
                    active: Some(id) == active,
                    urgent: false,
                    scratchpad: false,
                    active_window: None,
                    exec: String::new(),
                    actions: Vec::new(),
                    path: None,
                    pin_index: None,
                    // The tile draws its own number, not a glyph.
                    glyph: None,
                    pixmap: None,
                    window_meta: Vec::new(),
                    open_with: None,
                    media: None,
                    unread: 0,
                    downloading: 0,
                });
            }
        }

        if cfg.workspaces.scratchpad {
            let windows: Vec<Address> = self
                .clients
                .iter()
                .filter(|c| c.is_special() && !c.is_minimized())
                .map(|c| c.address.clone())
                .collect();
            items.push(DockItem {
                kind: ItemKind::Scratchpad,
                key: "__scratchpad".into(),
                label: "Scratchpad".into(),
                icon: String::new(),
                windows,
                pinned: false,
                // "Active" here means the drawer is open, which is exactly
                // what the indicator should say.
                active: self.special_is_open(),
                urgent: false,
                scratchpad: true,
                active_window: None,
                exec: String::new(),
                actions: Vec::new(),
                path: None,
                pin_index: None,
                glyph: Some(GLYPH_SCRATCHPAD.into()),
                pixmap: None,
                window_meta: Vec::new(),
                open_with: None,
                media: None,
                unread: 0,
                downloading: 0,
            });
        }

        items
    }

    #[allow(clippy::too_many_arguments)]
    fn make_item(
        &self,
        key: String,
        label: String,
        icon: String,
        windows: Vec<Address>,
        pinned: bool,
        exec: String,
        actions: Vec<crate::desktop::Action>,
    ) -> DockItem {
        let window_meta: Vec<WindowMeta> = windows
            .iter()
            .map(|w| {
                self.clients
                    .iter()
                    .find(|c| &c.address == w)
                    .map(WindowMeta::of)
                    .unwrap_or_default()
            })
            .collect();
        let active_window =
            self.focused.as_ref().filter(|f| windows.contains(f)).cloned();
        let active = active_window.is_some();
        let urgent = windows.iter().any(|w| self.urgent.contains(w));
        // Minimized windows sit on a special workspace too, but they are
        // not stashed in the scratchpad.
        let scratchpad = !windows.is_empty()
            && windows.iter().all(|w| {
                self.clients.iter().any(|c| &c.address == w && c.is_special())
            })
            && !window_meta.iter().all(|m| m.minimized);
        DockItem {
            kind: ItemKind::App,
            key,
            label,
            icon,
            windows,
            pinned,
            active,
            urgent,
            scratchpad,
            active_window,
            exec,
            actions,
            path: None,
            pin_index: None,
            glyph: None,
            pixmap: None,
            window_meta,
            open_with: None,
            media: None,
            unread: 0,
            downloading: 0,
        }
    }
}

/// The dock icon a notification belongs to.
///
/// A browser notification belongs to the web app for its site when there is
/// one — Gmail's mail badges Gmail, not Chromium — and otherwise to whatever
/// the sender names: its desktop id, then its app name.
pub fn notice_target<'a>(
    items: &'a [DockItem],
    notice: &crate::notices::Notice,
) -> Option<&'a DockItem> {
    let apps = || items.iter().filter(|i| i.kind == ItemKind::App);
    if let Some(origin) = &notice.origin {
        let host = |i: &DockItem| {
            matcher::webapp_host(&i.exec).or_else(|| {
                let (_, url) = matcher::webapp_from_class(&i.key)?;
                matcher::webapp_host(&format!("--app={url}"))
            })
        };
        // An exact site beats a loose one, whatever the dock order: with
        // Facebook and Messenger both pinned, each keeps its own mail.
        for exact in [true, false] {
            let site = apps().find(|i| host(i).is_some_and(|h| same_site(&h, origin, exact)));
            if site.is_some() {
                return site;
            }
        }
    }
    let named = |name: &str| {
        if name.is_empty() {
            return None;
        }
        apps().find(|i| {
            i.key.eq_ignore_ascii_case(name)
                || i.key.rsplit('.').next().is_some_and(|k| k.eq_ignore_ascii_case(name))
        })
    };
    named(&notice.desktop_entry)
        .or_else(|| named(&notice.app_name))
        .or_else(|| {
            apps().find(|i| !notice.app_name.is_empty() && i.label.eq_ignore_ascii_case(&notice.app_name))
        })
}

/// Whether two hosts are the same site as far as a user is concerned.
///
/// `www.` and `m.` never matter. Beyond `exact`, it is loose on purpose: a
/// web app is opened at one address and notifies from another — Omarchy's
/// Gmail opens `gmail.com` and mails from `mail.google.com` — so a subdomain
/// matches its parent, and a few well-known redirects are spelled out.
fn same_site(a: &str, b: &str, exact: bool) -> bool {
    fn norm(h: &str) -> &str {
        let h = h.trim_end_matches('.');
        h.strip_prefix("www.").or_else(|| h.strip_prefix("m.")).unwrap_or(h)
    }
    const ALIASES: [(&str, &str); 2] =
        [("gmail.com", "mail.google.com"), ("outlook.com", "outlook.live.com")];
    let (a, b) = (norm(&a.to_ascii_lowercase()).to_string(), norm(&b.to_ascii_lowercase()).to_string());
    if a == b {
        return true;
    }
    !exact
        && (a.ends_with(&format!(".{b}"))
        || b.ends_with(&format!(".{a}"))
            || ALIASES.iter().any(|(x, y)| (a == *x && b == *y) || (a == *y && b == *x)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(windows: &[&str], active: Option<&str>) -> DockItem {
        DockItem {
            kind: ItemKind::App,
            key: "k".into(),
            label: "k".into(),
            icon: "k".into(),
            windows: windows.iter().map(|w| Address::parse(w)).collect(),
            pinned: true,
            active: active.is_some(),
            urgent: false,
            scratchpad: false,
            active_window: active.map(Address::parse),
            exec: String::new(),
            actions: vec![],
            path: None,
            pin_index: None,
            glyph: None,
            pixmap: None,
            window_meta: Vec::new(),
            open_with: None,
            media: None,
            unread: 0,
            downloading: 0,
        }
    }

    fn meta(minimized: bool, recency: i32) -> WindowMeta {
        WindowMeta {
            title: String::new(),
            workspace: if minimized { "special:minimized".into() } else { "1".into() },
            minimized,
            home: None,
            recency,
            group: Vec::new(),
        }
    }

    /// An app whose windows are `(address, minimized, focus-history id)`.
    fn app(windows: &[(&str, bool, i32)], active: Option<&str>) -> DockItem {
        let addrs: Vec<&str> = windows.iter().map(|w| w.0).collect();
        let mut i = item(&addrs, active);
        i.window_meta = windows.iter().map(|&(_, m, r)| meta(m, r)).collect();
        i
    }

    #[test]
    fn clicking_the_focused_app_minimizes_its_focused_window() {
        let i = app(&[("a", false, 1), ("b", false, 0)], Some("b"));
        assert_eq!(i.click(), Click::Minimize(Address::parse("b")));
    }

    #[test]
    fn clicking_from_elsewhere_focuses_the_first_window_on_screen() {
        let i = app(&[("a", true, 0), ("b", false, 1)], None);
        assert_eq!(i.click(), Click::Focus(Address::parse("b")));
    }

    #[test]
    fn an_app_with_every_window_minimized_restores_the_last_one_minimized() {
        let i = app(&[("a", true, 5), ("b", true, 2)], None);
        assert_eq!(i.click(), Click::Restore(Address::parse("b")));
        assert!(i.all_minimized());
    }

    #[test]
    fn a_focused_window_that_is_minimized_is_not_minimized_again() {
        // The user opened special:minimized by hand and focused a window in it.
        let i = app(&[("a", true, 0), ("b", false, 1)], Some("a"));
        assert_eq!(i.click(), Click::Focus(Address::parse("b")));
        let i = app(&[("a", true, 0)], Some("a"));
        assert_eq!(i.click(), Click::Restore(Address::parse("a")));
    }

    #[test]
    fn a_stale_focus_does_not_minimize_anything() {
        let i = app(&[("a", false, 0)], Some("zz"));
        assert_eq!(i.click(), Click::Focus(Address::parse("a")));
    }

    #[test]
    fn a_drop_sends_a_window_on_screen_before_a_minimized_one() {
        // Focused wins, as long as it is not parked.
        let i = app(&[("a", false, 1), ("b", false, 0)], Some("b"));
        assert_eq!(i.drop_target(), Some(&Address::parse("b")));
        let i = app(&[("a", true, 0), ("b", false, 1)], Some("a"));
        assert_eq!(i.drop_target(), Some(&Address::parse("b")));
        let i = app(&[("a", true, 0), ("b", false, 1)], None);
        assert_eq!(i.drop_target(), Some(&Address::parse("b")));
    }

    #[test]
    fn a_drop_on_an_app_with_everything_minimized_takes_the_last_one() {
        let i = app(&[("a", true, 5), ("b", true, 2)], None);
        assert_eq!(i.drop_target(), Some(&Address::parse("b")));
        assert_eq!(item(&[], None).drop_target(), None);
    }

    #[test]
    fn nothing_running_launches() {
        assert_eq!(item(&[], None).click(), Click::Launch);
        assert!(!item(&[], None).all_minimized());
    }
}


#[cfg(test)]
mod focus_tests {
    use super::*;
    use crate::hypr::model::WorkspaceRef;
    use crate::hypr::Address;

    fn client(addr: &str) -> Client {
        Client {
            address: Address::parse(addr),
            class: "x".into(),
            title: "x".into(),
            initial_class: "x".into(),
            workspace: WorkspaceRef { id: 1, name: "1".into() },
            monitor: 0,
            pid: 1,
            floating: false,
            hidden: false,
            mapped: true,
            fullscreen: 0,
            at: (0, 0),
            size: (100, 100),
            focus_history_id: 0,
            tags: Vec::new(),
            grouped: Vec::new(),
        }
    }

    #[test]
    fn an_empty_workspace_really_clears_focus() {
        // Regression: focus used to be re-inferred from focusHistoryID, which
        // is global and kept naming a window on another workspace. That left
        // intelligent auto-hide stuck hidden on an empty workspace.
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![client("a")]);
        s.set_focused(Some(Address::parse("a")));
        assert!(s.focused_client().is_some());

        // Snapshot arrives while nothing is focused.
        s.set_clients(vec![client("a")]);
        s.set_focused(None);
        assert!(s.focused_client().is_none());
    }

    #[test]
    fn urgency_from_the_focused_window_is_refused_and_repeats_are_accepted() {
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![client("a"), client("b")]);
        s.set_focused(Some(Address::parse("a")));
        assert!(!s.set_urgent(Address::parse("a")));
        assert!(s.set_urgent(Address::parse("b")));
        // A second request is still news: it may pulse again.
        assert!(s.set_urgent(Address::parse("b")));
    }

    fn parked(a: &str) -> Client {
        let mut c = client(a);
        c.workspace = WorkspaceRef { id: -98, name: "special:minimized".into() };
        c.tags = vec!["omarchy-dock-home:1".into()];
        c
    }

    #[test]
    fn a_minimized_window_focused_from_outside_is_recalled_once() {
        // A launch-or-focus key focused it; Hyprland opened the whole
        // special:minimized workspace over the screen instead of bringing it back.
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![parked("a")]);
        s.set_focused(Some(Address::parse("a")));
        assert_eq!(s.recall().map(|c| c.address), Some(Address::parse("a")));
        // Until a snapshot shows it gone home, asking again sends nothing more.
        s.set_focused(Some(Address::parse("a")));
        assert!(s.recall().is_none());
    }

    #[test]
    fn a_window_that_went_home_can_be_recalled_on_its_next_trip() {
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![parked("a")]);
        s.set_focused(Some(Address::parse("a")));
        assert!(s.recall().is_some());
        s.set_clients(vec![client("a")]);
        s.set_clients(vec![parked("a")]);
        assert!(s.recall().is_some());
    }

    #[test]
    fn a_focused_window_on_screen_is_not_recalled() {
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![client("a")]);
        s.set_focused(Some(Address::parse("a")));
        assert!(s.recall().is_none());
    }

    #[test]
    fn the_window_minimized_last_is_the_most_recently_focused_one() {
        let parked = |a: &str, h: i32| {
            let mut c = client(a);
            c.workspace = WorkspaceRef { id: -98, name: "special:minimized".into() };
            c.focus_history_id = h;
            c
        };
        let mut s = DockState::new(vec![]);
        let mut shown = client("c");
        shown.focus_history_id = 0;
        s.set_clients(vec![parked("a", 3), parked("b", 1), shown]);
        assert_eq!(s.last_minimized().map(|c| c.address.clone()), Some(Address::parse("b")));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::config::Config;
    use crate::hypr::model::WorkspaceRef;
    use crate::hypr::Address;

    fn cfg(pinned: &[&str]) -> Config {
        let mut c = Config::default();
        c.items.pinned = pinned.iter().map(|s| s.to_string()).collect();
        c.items.show_trash = false;
        c.launcher.enabled = false;
        c
    }

    fn client(class: &str) -> Client {
        Client {
            address: Address::parse(class),
            class: class.into(),
            title: class.into(),
            initial_class: class.into(),
            workspace: WorkspaceRef { id: 1, name: "1".into() },
            monitor: 0,
            pid: 1,
            floating: false,
            hidden: false,
            mapped: true,
            fullscreen: 0,
            at: (0, 0),
            size: (10, 10),
            focus_history_id: 1,
            tags: Vec::new(),
            grouped: Vec::new(),
        }
    }

    fn kinds(items: &[DockItem]) -> Vec<ItemKind> {
        items.iter().map(|i| i.kind).collect()
    }

    #[test]
    fn an_app_with_only_minimized_windows_stays_in_the_dock() {
        let mut c = client("stranger");
        c.workspace = WorkspaceRef { id: -98, name: "special:minimized".into() };
        c.tags = vec!["omarchy-dock-home:4".into()];
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![c]);
        let items = s.items(&cfg(&[]));
        assert_eq!(items.len(), 1, "an unpinned minimized app must not vanish");
        assert!(items[0].all_minimized());
        assert!(!items[0].scratchpad, "minimized is not stashed in the scratchpad");
        assert_eq!(items[0].window_meta[0].home.as_deref(), Some("4"));
    }

    #[test]
    fn a_minimized_app_keeps_its_icon_when_running_apps_are_hidden() {
        // Its icon is the only way back to it.
        let mut parked = client("parked");
        parked.workspace = WorkspaceRef { id: -98, name: "special:minimized".into() };
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![parked, client("shown")]);
        let mut c = cfg(&[]);
        c.items.show_running = false;
        let items = s.items(&c);
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["parked"], "the minimized app only, not every running one");
    }

    #[test]
    fn running_apps_are_fenced_off_from_pinned_ones() {
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![client("stranger")]);
        let items = s.items(&cfg(&["pinned-app"]));
        assert_eq!(
            kinds(&items),
            vec![ItemKind::App, ItemKind::Separator, ItemKind::App]
        );
    }

    #[test]
    fn no_divider_when_there_is_nothing_to_divide() {
        let s = DockState::new(vec![]);
        // Pinned only: nothing running, so no trailing divider.
        assert_eq!(kinds(&s.items(&cfg(&["a", "b"]))), vec![ItemKind::App; 2]);

        // Running only: no pinned section, so no leading divider.
        let mut s2 = DockState::new(vec![]);
        s2.set_clients(vec![client("x")]);
        assert_eq!(kinds(&s2.items(&cfg(&[]))), vec![ItemKind::App]);
    }

    #[test]
    fn user_separators_are_placed_but_never_left_dangling() {
        let s = DockState::new(vec![]);
        let items = s.items(&cfg(&["a", SEPARATOR, "b"]));
        assert_eq!(
            kinds(&items),
            vec![ItemKind::App, ItemKind::Separator, ItemKind::App]
        );

        // A separator at either end divides nothing and is dropped.
        let items = s.items(&cfg(&[SEPARATOR, "a", SEPARATOR]));
        assert_eq!(kinds(&items), vec![ItemKind::App]);
    }

    #[test]
    fn launcher_leads_and_trash_trails_behind_a_divider() {
        let mut c = cfg(&["a"]);
        c.launcher.enabled = true;
        c.items.show_trash = true;
        let s = DockState::new(vec![]);
        assert_eq!(
            kinds(&s.items(&c)),
            vec![ItemKind::Launcher, ItemKind::App, ItemKind::Separator, ItemKind::Trash]
        );
    }

    #[test]
    fn adjacent_dividers_collapse_into_one() {
        let mut s = DockState::new(vec![]);
        s.set_clients(vec![client("stranger")]);
        // A trailing user separator lands exactly where the automatic divider
        // before the running section goes.
        let items = s.items(&cfg(&["a", SEPARATOR]));
        assert_eq!(
            kinds(&items),
            vec![ItemKind::App, ItemKind::Separator, ItemKind::App],
            "expected one divider, not two"
        );
    }

    #[test]
    fn a_dock_of_only_separators_collapses_rather_than_looping() {
        let s = DockState::new(vec![]);
        let items = s.items(&cfg(&[SEPARATOR, SEPARATOR]));
        assert!(items.len() <= 1, "got {:?}", kinds(&items));
    }

    fn drive(id: &str) -> drive::Drive {
        drive::Drive {
            id: id.into(),
            name: id.to_uppercase(),
            icon: "drive-removable-media-usb".into(),
            kind: drive::DeviceKind::UsbStick,
            can_eject: true,
            can_unmount: false,
            partitions: Vec::new(),
        }
    }

    fn folder() -> crate::config::Folder {
        crate::config::Folder {
            path: "/nonexistent/stack".into(),
            name: "Stack".into(),
            icon: String::new(),
            enabled: true,
        }
    }

    #[test]
    fn drives_sit_between_the_folders_and_trash() {
        let mut c = cfg(&["a"]);
        c.items.show_trash = true;
        c.items.folders = vec![folder()];
        let mut s = DockState::new(vec![]);
        s.set_drives(vec![drive("sdb1"), drive("sdc1")]);
        let items = s.items(&c);
        assert_eq!(
            kinds(&items),
            vec![
                ItemKind::App,
                ItemKind::Separator,
                ItemKind::Folder,
                ItemKind::Drive,
                ItemKind::Drive,
                ItemKind::Trash,
            ]
        );
        assert_eq!(drive_of(&items[3].key), Some("sdb1"));
        assert_eq!(items[3].label, "SDB1");
    }

    #[test]
    fn a_drives_only_tail_gets_its_divider() {
        let mut s = DockState::new(vec![]);
        s.set_drives(vec![drive("sdb1")]);
        assert_eq!(
            kinds(&s.items(&cfg(&["a"]))),
            vec![ItemKind::App, ItemKind::Separator, ItemKind::Drive]
        );
    }

    #[test]
    fn drives_are_hidden_when_turned_off() {
        let mut c = cfg(&["a"]);
        c.items.show_drives = false;
        let mut s = DockState::new(vec![]);
        s.set_drives(vec![drive("sdb1")]);
        assert_eq!(kinds(&s.items(&c)), vec![ItemKind::App]);
    }

    #[test]
    fn drives_take_no_hotkey_number() {
        let mut s = DockState::new(vec![]);
        s.set_drives(vec![drive("sdb1")]);
        let items = s.items(&cfg(&["a"]));
        assert_eq!(nth_app(&items, 0).map(|i| i.key.as_str()), Some("a"));
        assert!(nth_app(&items, 1).is_none());
    }
}

#[cfg(test)]
mod workspace_tests {
    #[test]
    fn app_numbering_skips_the_docks_furniture() {
        let mut launcher = separator();
        launcher.kind = ItemKind::Launcher;
        let app = |k: &str| {
            let mut i = separator();
            i.kind = ItemKind::App;
            i.key = k.into();
            i
        };
        let mut tile = separator();
        tile.kind = ItemKind::Command;
        tile.key = "cmd:themes".into();
        let mut trash = separator();
        trash.kind = ItemKind::Trash;

        let items = vec![launcher, separator(), app("a"), separator(), app("b"), tile, trash];
        let key = |n| nth_app(&items, n).map(|i| i.key.as_str());
        assert_eq!(key(0), Some("a"), "the first app, not the launcher");
        assert_eq!(key(1), Some("b"), "dividers do not shift the count");
        assert_eq!(key(2), Some("cmd:themes"), "a pinned command tile counts");
        assert_eq!(key(3), None, "Trash does not");
    }

    use super::*;
    use crate::hypr::model::{Monitor, WorkspaceRef};

    fn ws(id: i32, name: &str) -> Workspace {
        Workspace { id, name: name.into(), monitor: "eDP-1".into(), monitor_id: 0, windows: 0 }
    }

    fn client(addr: &str, ws_id: i32, ws_name: &str) -> Client {
        Client {
            address: Address::parse(addr),
            class: "app".into(),
            initial_class: "app".into(),
            title: "t".into(),
            workspace: WorkspaceRef { id: ws_id, name: ws_name.into() },
            monitor: 0,
            pid: 1,
            floating: false,
            hidden: false,
            mapped: true,
            fullscreen: 0,
            at: (0, 0),
            size: (100, 100),
            focus_history_id: 0,
            tags: Vec::new(),
            grouped: Vec::new(),
        }
    }

    fn monitor(active: i32) -> Monitor {
        Monitor {
            id: 0,
            name: "eDP-1".into(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            scale: 1.0,
            focused: true,
            active_workspace: WorkspaceRef { id: active, name: active.to_string() },
            special_workspace: None,
        }
    }

    fn state(clients: Vec<Client>, workspaces: Vec<Workspace>, active: i32) -> DockState {
        let mut s = DockState::new(Vec::new());
        s.set_clients(clients);
        s.set_workspaces(workspaces);
        s.set_monitors(vec![monitor(active)]);
        s
    }

    fn cfg(enabled: bool, show_empty: bool) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.workspaces.enabled = enabled;
        c.workspaces.show_empty = show_empty;
        // Only what Hyprland reports, so each test sees just the workspaces it
        // set up; the fixed row has tests of its own.
        c.workspaces.persistent = 0;
        c
    }

    fn labels(items: &[DockItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn the_first_five_always_have_a_tile_like_the_bar() {
        // Hyprland has destroyed every empty workspace; 3 is in use.
        let s = state(vec![client("0x1", 3, "3")], vec![ws(3, "3")], 3);
        let mut c = cfg(true, true);
        c.workspaces.persistent = 5;
        let items = s.workspace_items(&c);
        assert_eq!(labels(&items), ["1", "2", "3", "4", "5"]);
        // A filled-in tile still addresses its workspace, so a drop on it
        // sends the window there and Hyprland creates it.
        assert_eq!(workspace_of(&items[0].key), Some("1"));
        assert!(items[0].windows.is_empty());
        assert_eq!(items[2].windows.len(), 1);
        assert!(items[2].active);
    }

    #[test]
    fn workspaces_past_the_fixed_row_join_it_in_order() {
        let s = state(vec![client("0x1", 7, "7")], vec![ws(7, "7"), ws(2, "2")], 2);
        let mut c = cfg(true, true);
        c.workspaces.persistent = 5;
        assert_eq!(labels(&s.workspace_items(&c)), ["1", "2", "3", "4", "5", "7"]);
    }

    #[test]
    fn workspace_ten_is_labelled_zero_like_its_key() {
        let s = state(vec![client("0x1", 10, "10")], vec![ws(10, "10")], 10);
        let items = s.workspace_items(&cfg(true, true));
        assert_eq!(labels(&items), ["0"]);
        // The label is only the label: the tile still names workspace 10.
        assert_eq!(workspace_of(&items[0].key), Some("10"));
    }

    #[test]
    fn hiding_empty_workspaces_hides_the_fixed_row_too() {
        let s = state(vec![client("0x1", 3, "3")], vec![ws(3, "3")], 3);
        let mut c = cfg(true, false);
        c.workspaces.persistent = 5;
        assert_eq!(labels(&s.workspace_items(&c)), ["3"]);
    }

    #[test]
    fn workspace_tiles_carry_the_windows_on_them() {
        // Windows go into the item's own `windows`, which is what drives the
        // occupied styling and the count badge — no separate plumbing.
        let s = state(
            vec![client("0x1", 1, "1"), client("0x2", 2, "2"), client("0x3", 2, "2")],
            vec![ws(1, "1"), ws(2, "2")],
            1,
        );
        let items = s.workspace_items(&cfg(true, true));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].windows.len(), 1);
        assert_eq!(items[1].windows.len(), 2);
        // The one you are on is the active one.
        assert!(items[0].active);
        assert!(!items[1].active);
    }

    #[test]
    fn tiles_are_ordered_by_id_not_by_creation() {
        // Hyprland reports workspaces in creation order, which would make the
        // strip reshuffle itself as workspaces come and go.
        let s = state(vec![], vec![ws(3, "3"), ws(1, "1"), ws(2, "2")], 1);
        let items = s.workspace_items(&cfg(true, true));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["1", "2", "3"]);
    }

    #[test]
    fn empty_workspaces_can_be_hidden_but_the_current_one_never_is() {
        let s = state(vec![client("0x1", 2, "2")], vec![ws(1, "1"), ws(2, "2"), ws(3, "3")], 1);
        let items = s.workspace_items(&cfg(true, false));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // 2 has a window; 1 is empty but current; 3 is empty and elsewhere.
        assert_eq!(labels, ["1", "2"]);
    }

    #[test]
    fn special_workspaces_never_get_a_tile_of_their_own() {
        // The scratchpad has its own tile; listing it twice would be wrong.
        let s = state(vec![], vec![ws(1, "1"), ws(-99, "special:scratchpad")], 1);
        let items = s.workspace_items(&cfg(true, true));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "1");
    }

    #[test]
    fn the_scratchpad_tile_counts_what_is_stashed_in_it() {
        let mut c = cfg(false, true);
        c.workspaces.scratchpad = true;
        let s = state(
            vec![client("0x1", 1, "1"), client("0x2", -99, "special:scratchpad")],
            vec![ws(1, "1")],
            1,
        );
        let items = s.workspace_items(&c);
        assert_eq!(items.len(), 1, "no workspace strip, just the scratchpad");
        assert_eq!(items[0].kind, ItemKind::Scratchpad);
        assert_eq!(items[0].windows.len(), 1);
    }

    #[test]
    fn the_scratchpad_tile_does_not_count_minimized_windows() {
        let mut c = cfg(false, true);
        c.workspaces.scratchpad = true;
        let s = state(
            vec![client("0x2", -99, "special:scratchpad"), client("0x3", -98, "special:minimized")],
            vec![],
            1,
        );
        let items = s.workspace_items(&c);
        assert_eq!(items[0].windows.len(), 1);
    }

    #[test]
    fn a_workspace_tile_has_no_running_dot() {
        // Its brightness already says whether it holds windows; a dot repeats
        // that and reads as clutter.
        let s = state(vec![client("0x1", 1, "1")], vec![ws(1, "1")], 1);
        let items = s.workspace_items(&cfg(true, true));
        assert!(items[0].running());
        assert!(!items[0].shows_indicator());
    }

    #[test]
    fn a_tile_key_round_trips_to_its_workspace_name() {
        let s = state(vec![], vec![ws(1, "1"), ws(9, "code")], 1);
        let items = s.workspace_items(&cfg(true, true));
        assert_eq!(workspace_of(&items[0].key), Some("1"));
        // Named workspaces work too — Hyprland's focus dispatcher takes names.
        assert_eq!(workspace_of(&items[1].key), Some("code"));
        assert_eq!(workspace_of("chromium"), None);
    }

    #[test]
    fn fullscreen_counts_only_on_a_visible_workspace() {
        let mut full = client("0x1", 2, "2");
        full.fullscreen = 2;

        // Fullscreen on workspace 2 while looking at workspace 1: nothing is
        // covering the dock, so it must not hide.
        let s = state(vec![full.clone()], vec![ws(1, "1"), ws(2, "2")], 1);
        assert!(!s.has_fullscreen());

        // Same window, now the workspace you are on.
        let s = state(vec![full], vec![ws(1, "1"), ws(2, "2")], 2);
        assert!(s.has_fullscreen());
    }

    #[test]
    fn the_screensaver_is_recognised_by_its_class() {
        let s = state(vec![client("0x1", 1, "1")], vec![ws(1, "1")], 1);
        assert!(!s.screensaver_showing());

        let mut saver = client("0x2", 1, "1");
        saver.class = "org.omarchy.screensaver".into();
        let s = state(vec![client("0x1", 1, "1"), saver], vec![ws(1, "1")], 1);
        assert!(s.screensaver_showing());
    }
}

#[cfg(test)]
mod separator_key_tests {
    use super::*;

    #[test]
    fn reordering_inserts_rather_than_swapping() {
        let mut v = vec!["a", "b", "c", "d"];
        // Drag "a" to sit before "d".
        assert!(reorder_in_list(&mut v, 0, 3));
        assert_eq!(v, vec!["b", "c", "a", "d"]);

        // Drag "d" to the front.
        let mut v = vec!["a", "b", "c", "d"];
        assert!(reorder_in_list(&mut v, 3, 0));
        assert_eq!(v, vec!["d", "a", "b", "c"]);

        // Dropping onto itself changes nothing.
        let mut v = vec!["a", "b"];
        assert!(!reorder_in_list(&mut v, 1, 1));
        assert_eq!(v, vec!["a", "b"]);

        // Past the end clamps instead of panicking.
        let mut v = vec!["a", "b"];
        assert!(reorder_in_list(&mut v, 0, 2));
        assert_eq!(v, vec!["b", "a"]);
    }

    #[test]
    fn moving_clamps_at_the_ends_instead_of_wrapping() {
        let mut v = vec!["a", "b", "c"];
        assert!(move_in_list(&mut v, 0, 1));
        assert_eq!(v, vec!["b", "a", "c"]);

        // Already at the left end: no move, and no wrap to the far end.
        let mut v = vec!["a", "b", "c"];
        assert!(!move_in_list(&mut v, 0, -1));
        assert_eq!(v, vec!["a", "b", "c"]);

        // Same at the right end.
        let mut v = vec!["a", "b", "c"];
        assert!(!move_in_list(&mut v, 2, 1));
        assert_eq!(v, vec!["a", "b", "c"]);

        // Out of range is a no-op rather than a panic.
        let mut v = vec!["a"];
        assert!(!move_in_list(&mut v, 9, 1));
    }

    #[test]
    fn only_user_placed_separators_carry_an_editable_index() {
        assert_eq!(separator_pin_index("---:3"), Some(3));
        assert_eq!(separator_pin_index("---:0"), Some(0));
        // Automatic dividers are derived, so they are not editable.
        assert_eq!(separator_pin_index("---"), None);
        assert_eq!(separator_pin_index("chromium"), None);
    }

    /// A pinned app, as `items()` would produce it.
    fn app(key: &str, pin: usize) -> DockItem {
        DockItem {
            kind: ItemKind::App,
            key: key.into(),
            label: key.into(),
            icon: key.into(),
            windows: Vec::new(),
            pinned: true,
            active: false,
            urgent: false,
            scratchpad: false,
            active_window: None,
            exec: String::new(),
            actions: Vec::new(),
            path: None,
            pin_index: Some(pin),
            glyph: None,
            pixmap: None,
            window_meta: Vec::new(),
            open_with: None,
            media: None,
            unread: 0,
            downloading: 0,
        }
    }

    #[test]
    fn a_moved_separator_is_still_matched_to_its_own_widget() {
        // A separator's key encodes where it sits, so moving one changes its
        // key. Matching on that would fail and force a rebuild; matching on
        // the interchangeable class succeeds.
        let old = vec![app("a", 0), user_separator(1), app("b", 2)];
        let new = vec![user_separator(0), app("a", 1), app("b", 2)];
        assert_eq!(match_permutation(&old, &new), Some(vec![1, 0, 2]));
    }

    #[test]
    fn each_separator_claims_a_distinct_slot() {
        // Two user dividers plus the automatic one. Every separator must map
        // to a different old slot: mapping them all to the first would leave
        // widgets bound to the wrong items, which is what made dragging one
        // icon move another.
        let old = vec![
            app("a", 0),
            user_separator(1),
            user_separator(2),
            separator(),
            app("b", 3),
        ];
        let new = vec![
            user_separator(0),
            app("a", 1),
            user_separator(2),
            separator(),
            app("b", 3),
        ];
        let from = match_permutation(&old, &new).expect("still a permutation");
        assert_eq!(from, vec![1, 0, 2, 3, 4]);

        let mut seen = from.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), from.len(), "no old slot may be claimed twice");
    }

    #[test]
    fn a_user_separator_never_matches_an_automatic_divider() {
        // The automatic divider is derived from where the pinned list ends, so
        // it has no position to rewrite. Letting a draggable separator bind to
        // it would make a drag rewrite an entry that does not exist.
        let old = vec![app("a", 0), user_separator(1)];
        let new = vec![app("a", 0), separator()];
        assert_eq!(match_permutation(&old, &new), None);
    }

    #[test]
    fn a_changed_item_set_falls_back_to_a_rebuild() {
        // Different apps, not a reordering: the caller needs new widgets.
        let old = vec![app("a", 0), app("b", 1)];
        let new = vec![app("a", 0), app("c", 1)];
        assert_eq!(match_permutation(&old, &new), None);

        // A different length is never a permutation either.
        let new = vec![app("a", 0)];
        assert_eq!(match_permutation(&old, &new), None);
    }

    #[test]
    fn the_permutation_maps_new_positions_back_to_old_ones() {
        // Dragging the last pinned app to the front.
        let old = vec![app("a", 0), app("b", 1), app("c", 2)];
        let new = vec![app("c", 0), app("a", 1), app("b", 2)];
        let from = match_permutation(&old, &new).expect("a permutation");
        assert_eq!(from, vec![2, 0, 1]);

        // Reading old through `from` must reproduce new, which is exactly the
        // guarantee the widget permutation relies on.
        let rebuilt: Vec<&str> =
            from.iter().map(|&i| old[i].key.as_str()).collect();
        let want: Vec<&str> = new.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(rebuilt, want);
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;
    use crate::notices::Notice;

    fn app(key: &str, label: &str, exec: &str) -> DockItem {
        DockItem {
            kind: ItemKind::App,
            key: key.into(),
            label: label.into(),
            icon: String::new(),
            windows: Vec::new(),
            pinned: true,
            active: false,
            urgent: false,
            scratchpad: false,
            active_window: None,
            exec: exec.into(),
            actions: Vec::new(),
            path: None,
            pin_index: None,
            glyph: None,
            pixmap: None,
            window_meta: Vec::new(),
            open_with: None,
            media: None,
            unread: 0,
            downloading: 0,
        }
    }

    /// Roughly this machine's dock.
    fn dock() -> Vec<DockItem> {
        vec![
            app("chromium", "Chromium", "chromium"),
            app("spotify", "Spotify", "spotify"),
            app("Gmail", "Gmail", r#"omarchy-launch-webapp "https://gmail.com""#),
            app("Outlook", "Outlook", r#"omarchy-launch-webapp "https://outlook.live.com/mail/""#),
            app("Facebook", "Facebook", r#"omarchy-launch-webapp "https://facebook.com""#),
            app("Messenger", "Messenger", r#"omarchy-launch-webapp "https://messenger.com""#),
        ]
    }

    fn from_site(host: &str) -> Notice {
        Notice { app_name: "Chromium".into(), desktop_entry: "chromium".into(), origin: Some(host.into()) }
    }

    fn target(n: &Notice) -> Option<String> {
        let items = dock();
        notice_target(&items, n).map(|i| i.key.clone())
    }

    #[test]
    fn a_site_notification_badges_its_web_app_not_the_browser() {
        // Gmail opens gmail.com and mails from mail.google.com.
        assert_eq!(target(&from_site("mail.google.com")).as_deref(), Some("Gmail"));
        assert_eq!(target(&from_site("outlook.live.com")).as_deref(), Some("Outlook"));
        assert_eq!(target(&from_site("www.messenger.com")).as_deref(), Some("Messenger"));
    }

    #[test]
    fn neighbouring_sites_keep_their_own_badges() {
        // Facebook sits before Messenger; neither may take the other's mail.
        assert_eq!(target(&from_site("www.facebook.com")).as_deref(), Some("Facebook"));
        assert_eq!(target(&from_site("messenger.com")).as_deref(), Some("Messenger"));
    }

    #[test]
    fn a_site_with_no_web_app_badges_the_browser() {
        assert_eq!(target(&from_site("github.com")).as_deref(), Some("chromium"));
    }

    #[test]
    fn an_app_is_found_by_desktop_id_then_by_name() {
        let n = Notice { app_name: "Spotify".into(), ..Default::default() };
        assert_eq!(target(&n).as_deref(), Some("spotify"));
        let n = Notice { app_name: "x".into(), desktop_entry: "spotify".into(), origin: None };
        assert_eq!(target(&n).as_deref(), Some("spotify"));
        let n = Notice { app_name: "sudo".into(), ..Default::default() };
        assert_eq!(target(&n), None);
    }

    #[test]
    fn counts_grow_until_the_app_is_looked_at() {
        let mut s = DockState::new(Vec::new());
        let mut items = dock();
        let mail = from_site("mail.google.com");
        assert!(s.note_notice(&items, &mail));
        assert!(s.note_notice(&items, &mail));
        assert_eq!(s.unread.get("Gmail"), Some(&2));

        // Nothing is cleared while Gmail is in the background...
        assert!(!s.clear_seen(&items));
        // ...and focusing it reads them.
        items[2].active = true;
        assert!(s.clear_seen(&items));
        assert_eq!(s.unread.get("Gmail"), None);
        // Mail arriving while you are looking at it is never unread.
        assert!(!s.note_notice(&items, &mail));
    }

    fn action(id: &str, exec: &str) -> crate::desktop::Action {
        crate::desktop::Action { id: id.into(), name: id.into(), exec: exec.into() }
    }

    #[test]
    fn middle_click_prefers_the_entrys_new_window_action() {
        let mut chromium = app("chromium", "Chromium", "/usr/bin/chromium");
        chromium.actions = vec![
            action("new-private-window", "/usr/bin/chromium --incognito"),
            action("new-window", "/usr/bin/chromium --new-window %U"),
        ];
        assert_eq!(chromium.new_window_command().as_deref(), Some("/usr/bin/chromium --new-window"));

        let mut code = app("code", "Code", "code");
        code.actions = vec![action("new-empty-window", "code --new-window %F")];
        assert_eq!(code.new_window_command().as_deref(), Some("code --new-window"));
    }

    #[test]
    fn middle_click_otherwise_launches_again() {
        let gmail = app("Gmail", "Gmail", r#"omarchy-launch-webapp "https://gmail.com""#);
        assert_eq!(gmail.new_window_command(), Some(gmail.exec.clone()));
        // A class-pinned web app's click command only raises its window.
        let pinned = app(
            "chrome-mail.google.com__-Default",
            "mail.google.com",
            "omarchy launch or focus webapp 'chrome-mail.google.com__-Default' 'https://mail.google.com'",
        );
        assert_eq!(
            pinned.new_window_command().as_deref(),
            Some("omarchy-launch-webapp 'https://mail.google.com'")
        );
    }

    #[test]
    fn only_an_omarchy_web_app_with_a_launcher_can_be_removed() {
        let dir = std::env::temp_dir().join(format!("omarchy-dock-webapps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Gmail.desktop"), "").unwrap();
        std::fs::write(dir.join("spotify.desktop"), "").unwrap();

        let gmail = app("Gmail", "Gmail", r#"omarchy-launch-webapp "https://gmail.com""#);
        assert_eq!(gmail.omarchy_webapp_in(&dir).as_deref(), Some("Gmail"));
        // A regular app is never offered, launcher or not.
        assert_eq!(app("spotify", "Spotify", "spotify").omarchy_webapp_in(&dir), None);
        // A web app without a launcher (pinned from its window) has nothing
        // for the script to remove.
        let adhoc = app("Outlook", "Outlook", r#"omarchy-launch-webapp "https://outlook.com""#);
        assert_eq!(adhoc.omarchy_webapp_in(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_removal_command_quotes_the_name() {
        let q = matcher::shell_quote("Bob's App");
        assert_eq!(format!("omarchy-webapp-remove {q}"), r#"omarchy-webapp-remove 'Bob'\''s App'"#);
    }

    #[test]
    fn middle_click_means_nothing_on_furniture() {
        let mut trash = app("__trash", "Trash", "");
        trash.kind = ItemKind::Trash;
        assert_eq!(trash.new_window_command(), None);
    }

    #[test]
    fn an_unread_count_takes_the_badge_over_the_window_count() {
        let mut a = app("code", "Code", "code");
        a.windows = vec![Address("0x1".into()), Address("0x2".into())];
        assert_eq!(a.badge(), Some(2));
        a.unread = 5;
        assert_eq!(a.badge(), Some(5));
    }
}

#[cfg(test)]
mod recording_tests {
    use super::*;

    #[test]
    fn a_stop_button_sits_beside_the_launcher_only_while_recording() {
        let cfg = crate::config::Config::default();
        let mut s = DockState::new(Vec::new());
        assert!(!s.items(&cfg).iter().any(|i| i.kind == ItemKind::Recording));

        s.set_recording(true);
        let items = s.items(&cfg);
        let at = items.iter().position(|i| i.kind == ItemKind::Recording).unwrap();
        assert_eq!(items[at - 1].kind, ItemKind::Launcher);
        assert_eq!(items[at].exec, STOP_RECORDING);
        // Not a pin: it cannot be dragged, and a restart does not remember it.
        assert_eq!(items[at].pin_index, None);
    }
}
