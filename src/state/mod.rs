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

pub mod matcher;

use crate::desktop::Entry;
use crate::hypr::model::{Client, Monitor};
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
    /// launcher, folders and Trash — which have nothing to reorder.
    pub pin_index: Option<usize>,
}

impl DockItem {
    pub fn running(&self) -> bool {
        !self.windows.is_empty()
    }

    /// Separators take no input and show no indicator.
    pub fn interactive(&self) -> bool {
        self.kind != ItemKind::Separator
    }

    /// Count shown as a badge; `None` below two windows.
    pub fn badge(&self) -> Option<usize> {
        (self.windows.len() > 1).then_some(self.windows.len())
    }

    /// Which window a left-click should focus.
    ///
    /// Clicking an app that already holds focus advances to its next window,
    /// so repeated clicks cycle — the behaviour a dock icon is expected to
    /// have. Clicking an app that does *not* hold focus jumps to its first
    /// window rather than resuming the cycle, so a click from elsewhere is
    /// predictable instead of landing on wherever the cycle last stopped.
    pub fn click_target(&self) -> Option<&Address> {
        if self.windows.is_empty() {
            return None;
        }
        // A stale focus (naming a window that closed between events) falls
        // through to the first window: doing nothing on click would be worse
        // than being slightly arbitrary.
        let next = self
            .active_window
            .as_ref()
            .and_then(|current| self.windows.iter().position(|w| w == current))
            .map(|at| (at + 1) % self.windows.len())
            .unwrap_or(0);
        self.windows.get(next)
    }
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
    }
}

pub struct DockState {
    matcher: Matcher,
    clients: Vec<Client>,
    monitors: Vec<Monitor>,
    focused: Option<Address>,
    urgent: Vec<Address>,
}

impl DockState {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            matcher: Matcher::build(entries),
            clients: Vec::new(),
            monitors: Vec::new(),
            focused: None,
            urgent: Vec::new(),
        }
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    /// Replace the window set wholesale, as after a snapshot or reconnect.
    pub fn set_clients(&mut self, clients: Vec<Client>) {
        self.clients = clients;
        // An urgent window that has since closed must not stay urgent.
        self.urgent.retain(|a| self.clients.iter().any(|c| &c.address == a));
    }

    pub fn set_monitors(&mut self, monitors: Vec<Monitor>) {
        self.monitors = monitors;
    }

    pub fn monitor_by_name(&self, name: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.name == name)
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

    pub fn set_urgent(&mut self, addr: Address) {
        if self.focused.as_ref() != Some(&addr) && !self.urgent.contains(&addr) {
            self.urgent.push(addr);
        }
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
            });
        }

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
            let entry = self.matcher.by_id(id);
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

            let mut pinned_item = self.make_item(
                id.clone(),
                entry.map(|e| e.name.clone()).unwrap_or_else(|| id.clone()),
                entry.map(|e| e.icon.clone()).filter(|i| !i.is_empty()).unwrap_or_else(|| id.clone()),
                windows,
                true,
                entry.map(|e| e.command()).unwrap_or_default(),
                entry.map(|e| e.actions.clone()).unwrap_or_default(),
            );
            pinned_item.pin_index = Some(pin_index);
            items.push(pinned_item);
        }

        // Everything appended from here is a distinct section.
        let pinned_end = items.len();

        if show_running {
            // Group leftovers by matched entry so multiple windows of one app
            // collapse into a single icon.
            type Group = (String, String, String, Vec<Address>, String, Vec<crate::desktop::Action>);
            let mut groups: Vec<Group> = Vec::new();
            for (i, c) in self.clients.iter().enumerate() {
                if claimed[i] || c.is_special() {
                    continue;
                }
                let entry = self.matcher.match_class(c.match_key());
                let key = entry.map(|e| e.id.clone()).unwrap_or_else(|| c.match_key().to_string());
                let label =
                    entry.map(|e| e.name.clone()).unwrap_or_else(|| c.class.clone());
                let icon = entry
                    .map(|e| e.icon.clone())
                    .filter(|i| !i.is_empty())
                    .unwrap_or_else(|| c.class.clone());

                match groups.iter_mut().find(|g| g.0 == key) {
                    Some(g) => g.3.push(c.address.clone()),
                    None => groups.push((
                        key,
                        label,
                        icon,
                        vec![c.address.clone()],
                        entry.map(|e| e.command()).unwrap_or_default(),
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
                items.push(self.make_item(key, label, icon, windows, false, exec, actions));
            }
        }

        // Stacks and Trash form the dock's tail section, as on macOS.
        let tail_start = items.len();

        for folder in cfg.items.folders.iter().filter(|f| f.enabled) {
            let path = crate::config::expand_tilde(&folder.path);
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
                path: Some(path),
                pin_index: None,
            });
        }

        if cfg.items.show_trash {
            items.push(DockItem {
                kind: ItemKind::Trash,
                key: "__trash".into(),
                label: "Trash".into(),
                // Icon switches to user-trash-full when it has contents.
                icon: if crate::stacks::trash_is_empty() {
                    "user-trash".into()
                } else {
                    "user-trash-full".into()
                },
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
            });
        }

        if items.len() > tail_start && tail_start > 0 {
            items.insert(tail_start, separator());
        }

        // Two dividers in a row divide nothing between them. This happens
        // whenever a user separator sits where an automatic one is also
        // inserted, e.g. a trailing "---" meeting the folders divider.
        items.dedup_by(|a, b| a.kind == ItemKind::Separator && b.kind == ItemKind::Separator);

        // A separator at either end divides nothing either.
        while items.first().is_some_and(|i| i.kind == ItemKind::Separator) {
            items.remove(0);
        }
        while items.len() > 1 && items.last().is_some_and(|i| i.kind == ItemKind::Separator) {
            items.pop();
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
        let active_window =
            self.focused.as_ref().filter(|f| windows.contains(f)).cloned();
        let active = active_window.is_some();
        let urgent = windows.iter().any(|w| self.urgent.contains(w));
        let scratchpad = !windows.is_empty()
            && windows.iter().all(|w| {
                self.clients.iter().any(|c| &c.address == w && c.is_special())
            });
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
        }
    }
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
        }
    }

    #[test]
    fn repeated_clicks_cycle_and_wrap() {
        let i = item(&["a", "b", "c"], Some("a"));
        assert_eq!(i.click_target(), Some(&Address::parse("b")));
        let i = item(&["a", "b", "c"], Some("c"));
        // Wraps back to the first rather than stopping at the end.
        assert_eq!(i.click_target(), Some(&Address::parse("a")));
    }

    #[test]
    fn clicking_from_elsewhere_jumps_to_the_first_window() {
        let i = item(&["a", "b", "c"], None);
        assert_eq!(i.click_target(), Some(&Address::parse("a")));
    }

    #[test]
    fn single_window_click_is_idempotent() {
        // Focusing the only window again must not wrap to nothing.
        let i = item(&["a"], Some("a"));
        assert_eq!(i.click_target(), Some(&Address::parse("a")));
    }

    #[test]
    fn nothing_running_has_no_target_so_the_caller_launches() {
        assert_eq!(item(&[], None).click_target(), None);
    }

    #[test]
    fn stale_focus_falls_back_to_the_first_window() {
        // Focus can name a window that closed between events. Returning
        // nothing would make the click silently do nothing.
        let i = item(&["a", "b"], Some("zz"));
        assert_eq!(i.click_target(), Some(&Address::parse("a")));
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
        }
    }

    fn kinds(items: &[DockItem]) -> Vec<ItemKind> {
        items.iter().map(|i| i.kind).collect()
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
