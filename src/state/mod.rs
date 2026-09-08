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

/// One rendered dock item.
#[derive(Debug, Clone)]
pub struct DockItem {
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
}

impl DockItem {
    pub fn running(&self) -> bool {
        !self.windows.is_empty()
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

    /// Build the ordered item list: pinned entries first in their configured
    /// order, then running-but-unpinned apps.
    pub fn items(&self, pinned: &[String], show_running: bool) -> Vec<DockItem> {
        let mut items: Vec<DockItem> = Vec::new();
        // Which clients have been claimed by a pinned slot.
        let mut claimed: Vec<bool> = vec![false; self.clients.len()];

        for id in pinned {
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
            for (key, label, icon, windows, exec, actions) in groups {
                items.push(self.make_item(key, label, icon, windows, false, exec, actions));
            }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(windows: &[&str], active: Option<&str>) -> DockItem {
        DockItem {
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
