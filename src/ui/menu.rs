//! Right-click context menu for a dock item.
//!
//! Built from plain widgets in a `gtk::Popover` rather than `PopoverMenu` with
//! a `GMenu` model: the rows are dynamic (one per open window, one per desktop
//! action), and a menu model would mean registering and tearing down an action
//! group on every rebuild for no gain.

use gtk4 as gtk;

use gtk::prelude::*;

use crate::runtime::DockCommand;
use crate::state::DockItem;

/// What the menu asks the app to do after it closes.
#[derive(Debug, Clone)]
pub enum MenuAction {
    Command(DockCommand),
    /// Add or remove this item from the pinned list, and persist it.
    SetPinned { key: String, pinned: bool },
    /// Something on disk changed (trash emptied, file deleted); re-read state
    /// so the Trash icon and stack contents catch up.
    Rescan,
}

/// Build (but do not show) the context menu for `item`.
pub fn build<F>(item: &DockItem, on_action: F) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");

    // ── open windows ────────────────────────────────────────────────────────
    // Listing them lets a multi-window app be steered directly, instead of
    // only cycling blindly with repeated clicks.
    if item.windows.len() > 1 {
        list.append(&heading(&format!("{} windows", item.windows.len())));
        for (i, addr) in item.windows.iter().enumerate() {
            let label = format!("Window {}", i + 1);
            let a = addr.clone();
            let cb = on_action.clone();
            let pop = popover.clone();
            list.append(&row(&label, move || {
                cb(MenuAction::Command(DockCommand::Focus(a.clone())));
                pop.popdown();
            }));
        }
        list.append(&separator());
    }

    // ── desktop actions ─────────────────────────────────────────────────────
    // e.g. Chromium's "New Window" / "New Incognito Window".
    if !item.actions.is_empty() {
        for action in &item.actions {
            let exec = crate::desktop::strip_field_codes(&action.exec);
            if exec.is_empty() {
                continue;
            }
            let cb = on_action.clone();
            let pop = popover.clone();
            list.append(&row(&action.name, move || {
                cb(MenuAction::Command(DockCommand::Exec(exec.clone())));
                pop.popdown();
            }));
        }
        list.append(&separator());
    }

    // ── pinning ─────────────────────────────────────────────────────────────
    if item.key != "__trash" {
        let pinned = item.pinned;
        let key = item.key.clone();
        let cb = on_action.clone();
        let pop = popover.clone();
        let label = if pinned { "Remove from Dock" } else { "Keep in Dock" };
        list.append(&row(label, move || {
            cb(MenuAction::SetPinned { key: key.clone(), pinned: !pinned });
            pop.popdown();
        }));
    }

    // ── quit ────────────────────────────────────────────────────────────────
    if item.running() {
        let windows = item.windows.clone();
        let cb = on_action.clone();
        let pop = popover.clone();
        let label = if windows.len() > 1 { "Quit All" } else { "Quit" };
        list.append(&row(label, move || {
            // Closing every window is what "Quit" means for a dock item; the
            // app exits once its last window goes.
            for w in &windows {
                cb(MenuAction::Command(DockCommand::Close(w.clone())));
            }
            pop.popdown();
        }));
    }

    popover.set_child(Some(&list));
    popover
}

fn row<F: Fn() + 'static>(label: &str, on_click: F) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("dock-menu-item");
    button.set_has_frame(false);
    // Left-align like a real menu; GtkButton centres by default.
    if let Some(l) = button.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    button.connect_clicked(move |_| on_click());
    button
}

fn heading(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dock-menu-heading");
    l.set_xalign(0.0);
    l
}

fn separator() -> gtk::Separator {
    let s = gtk::Separator::new(gtk::Orientation::Horizontal);
    s.add_css_class("dock-menu-sep");
    s
}
