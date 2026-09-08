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
            });
        }

        // Which clients have been claimed by a pinned slot.
        let mut claimed: Vec<bool> = vec![false; self.clients.len()];

        for id in pinned {
            if id.trim() == SEPARATOR {
                items.push(separator());
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

            items.push(self.make_item(
                id.clone(),
                entry.map(|e| e.name.clone()).unwrap_or_else(|| id.clone()),
                entry.map(|e| e.icon.clone()).filter(|i| !i.is_empty()).unwrap_or_else(|| id.clone()),
                windows,
                true,
                entry.map(|e| e.command()).unwrap_or_default(),
                entry.map(|e| e.actions.clone()).unwrap_or_default(),
            ));
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

        for folder in &cfg.items.folders {
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
            });
        }

        if items.len() > tail_start && tail_start > 0 {
            items.insert(tail_start, separator());
        }

        // A separator at either end divides nothing.
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
    fn a_dock_of_only_separators_collapses_rather_than_looping() {
        let s = DockState::new(vec![]);
        let items = s.items(&cfg(&[SEPARATOR, SEPARATOR]));
        assert!(items.len() <= 1, "got {:?}", kinds(&items));
    }
}
