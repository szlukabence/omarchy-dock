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
use crate::hypr::model::Client;
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
}

impl DockItem {
    pub fn running(&self) -> bool {
        !self.windows.is_empty()
    }

    /// Count shown as a badge; `None` below two windows.
    pub fn badge(&self) -> Option<usize> {
        (self.windows.len() > 1).then_some(self.windows.len())
    }
}

pub struct DockState {
    matcher: Matcher,
    clients: Vec<Client>,
    focused: Option<Address>,
    urgent: Vec<Address>,
}

impl DockState {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            matcher: Matcher::build(entries),
            clients: Vec::new(),
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
            ));
        }

        if show_running {
            // Group leftovers by matched entry so multiple windows of one app
            // collapse into a single icon.
            let mut groups: Vec<(String, String, String, Vec<Address>)> = Vec::new();
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
                    None => groups.push((key, label, icon, vec![c.address.clone()])),
                }
            }
            for (key, label, icon, windows) in groups {
                items.push(self.make_item(key, label, icon, windows, false));
            }
        }

        items
    }

    fn make_item(
        &self,
        key: String,
        label: String,
        icon: String,
        windows: Vec<Address>,
        pinned: bool,
    ) -> DockItem {
        let active = self.focused.as_ref().is_some_and(|f| windows.contains(f));
        let urgent = windows.iter().any(|w| self.urgent.contains(w));
        let scratchpad = !windows.is_empty()
            && windows.iter().all(|w| {
                self.clients.iter().any(|c| &c.address == w && c.is_special())
            });
        DockItem { key, label, icon, windows, pinned, active, urgent, scratchpad }
    }
}
