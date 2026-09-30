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
    /// Raise one of the Omarchy shell's own surfaces.
    OpenSurface(crate::omarchy::Surface),
    Command(DockCommand),
    /// Add or remove this item from the pinned list, and persist it.
    SetPinned { key: String, pinned: bool },
    /// Something on disk changed (trash emptied, file deleted); re-read state
    /// so the Trash icon and stack contents catch up.
    Rescan,
    /// Move a pinned entry to a new position, as a drag-and-drop does.
    ReorderPin { from: usize, to: usize },
    /// Drop an entry from the pinned list.
    RemovePin { index: usize },
    /// Open one partition of a removable drive in the file manager,
    /// mounting it first. Carries the partition's id.
    DriveOpen(String),
    /// Eject a whole removable device, or unmount everything on it. Carries
    /// the device's id.
    DriveEject(String),
}

/// Context menu for a user-placed separator.
///
/// Separators have no windows, no actions and nothing to launch, so removing
/// them is all their menu needs to offer — position is handled by dragging.
pub fn build_separator<F>(pin_index: usize, on_action: F) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");
    list.append(&heading("Separator"));
    {
        let cb = on_action.clone();
        let pop = popover.clone();
        list.append(&row("Remove separator", move || {
            cb(MenuAction::RemovePin { index: pin_index });
            pop.popdown();
        }));
    }

    popover.set_child(Some(&list));
    popover
}

/// Menu for a removable device: open a partition, and, with `eject`, eject
/// the whole device or unmount everything on it.
///
/// Without `eject` it is the partition picker a left-click opens on a device
/// with more than one. Built from the device as it is now, not as the dock
/// last drew it, so it offers exactly what can be done.
pub fn build_drive<F>(drive: &crate::drives::Drive, on_action: F, eject: bool) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");
    list.append(&heading(&drive.name));
    let single = drive.partitions.len() == 1;
    for part in &drive.partitions {
        let label = match (single, part.mounted) {
            (true, _) => "Open".to_string(),
            (false, false) => format!("Open {}", part.name),
            (false, true) => format!("Open {} (mounted)", part.name),
        };
        let cb = on_action.clone();
        let pop = popover.clone();
        let id = part.id.clone();
        list.append(&row(&label, move || {
            cb(MenuAction::DriveOpen(id.clone()));
            pop.popdown();
        }));
    }
    let undo = if drive.can_eject {
        Some("Eject")
    } else if drive.can_unmount {
        Some("Unmount")
    } else {
        None
    };
    if let Some(label) = undo.filter(|_| eject) {
        list.append(&separator());
        let cb = on_action.clone();
        let pop = popover.clone();
        let id = drive.id.clone();
        list.append(&row(label, move || {
            cb(MenuAction::DriveEject(id.clone()));
            pop.popdown();
        }));
    }

    popover.set_child(Some(&list));
    popover
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

    // ── now playing ─────────────────────────────────────────────────────────
    // Transport controls for the player that belongs to this app — the one
    // place in Omarchy they sit on the app itself.
    if let Some(player) = &item.media {
        list.append(&now_playing(player, &popover, &on_action));
        list.append(&separator());
    }

    // ── open windows ────────────────────────────────────────────────────────
    // One row per window: its title and workspace, so an app open on several
    // workspaces can be steered directly instead of cycled through blindly.
    // Each row carries a minimize (or restore) button and a move button:
    // windows are organised by workspace in Omarchy, and minimizing parks one
    // out of the way until it is wanted.
    if !item.windows.is_empty() {
        let n = item.windows.len();
        list.append(&heading(if n == 1 { "Window" } else { "Windows" }));
        for (i, addr) in item.windows.iter().enumerate() {
            let meta = item.window_meta.get(i).cloned().unwrap_or_default();
            list.append(&window_row(addr, &meta, &item.label, &popover, &on_action));
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

    // ── remove web app ──────────────────────────────────────────────────────
    // Deletes the launcher and its icon, so it asks first: the first click
    // arms the row, the second removes. In the menu itself rather than a
    // dialog, because a dialog for one yes/no is heavier than the action.
    if let Some(cmd) = item.remove_webapp_command() {
        let armed = std::rc::Rc::new(std::cell::Cell::new(false));
        let (key, pinned, label) = (item.key.clone(), item.pinned, item.label.clone());
        let cb = on_action.clone();
        let pop = popover.clone();
        let button = gtk::Button::new();
        let text = gtk::Label::new(Some("Remove Web App…"));
        text.set_xalign(0.0);
        button.set_child(Some(&text));
        button.add_css_class("dock-menu-item");
        button.set_has_frame(false);
        button.connect_clicked(move |b| {
            if !armed.replace(true) {
                text.set_text(&format!("Click again to remove {label}"));
                b.add_css_class("dock-menu-danger");
                return;
            }
            // Unpinned first, so the dock does not keep a tile for an app
            // whose launcher is about to vanish.
            if pinned {
                cb(MenuAction::SetPinned { key: key.clone(), pinned: false });
            }
            cb(MenuAction::Command(DockCommand::Exec(cmd.clone())));
            pop.popdown();
        });
        list.append(&button);
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

/// Track title, artist, and previous / play-pause / next.
fn now_playing<F>(player: &crate::media::Player, popover: &gtk::Popover, on_action: &F) -> gtk::Box
where
    F: Fn(MenuAction) + Clone + 'static,
{
    use crate::media::Action;

    let block = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let title = if player.title.is_empty() { player.identity.clone() } else { player.title.clone() };
    let heading_text = if player.artist.is_empty() { title } else { format!("{title} — {}", player.artist) };
    let h = heading(&heading_text);
    h.set_ellipsize(gtk::pango::EllipsizeMode::End);
    h.set_max_width_chars(34);
    block.append(&h);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    controls.add_css_class("dock-move-grid");
    controls.set_halign(gtk::Align::Center);
    let buttons = [
        ("\u{f048}", "Previous", Action::Previous, player.can_previous),
        (if player.playing { "\u{f04c}" } else { "\u{f04b}" },
         if player.playing { "Pause" } else { "Play" }, Action::PlayPause, true),
        ("\u{f051}", "Next", Action::Next, player.can_next),
    ];
    for (glyph, tip, action, enabled) in buttons {
        let b = gtk::Button::with_label(glyph);
        b.add_css_class("dock-menu-item");
        b.add_css_class("dock-glyph");
        b.add_css_class("dock-move-target");
        b.set_has_frame(false);
        b.set_tooltip_text(Some(tip));
        b.set_sensitive(enabled);
        let bus = player.bus.clone();
        let cb = on_action.clone();
        let pop = popover.clone();
        b.connect_clicked(move |_| {
            cb(MenuAction::Command(DockCommand::Media { bus: bus.clone(), action }));
            pop.popdown();
        });
        controls.append(&b);
    }
    block.append(&controls);
    block
}

/// Workspaces offered as move targets: the nine Omarchy binds to SUPER+1..9.
/// Hyprland creates a workspace on first use, so all nine are always valid.
const MOVE_TARGETS: u32 = 9;

/// One window in the context menu: focus it, or move it elsewhere.
fn window_row<F>(
    addr: &crate::hypr::Address,
    meta: &crate::state::WindowMeta,
    app_label: &str,
    popover: &gtk::Popover,
    on_action: &F,
) -> gtk::Box
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);

    let title = if meta.title.is_empty() { app_label.to_string() } else { meta.title.clone() };
    let focus = row("", {
        let (a, m) = (addr.clone(), meta.clone());
        let cb = on_action.clone();
        let pop = popover.clone();
        move || {
            cb(MenuAction::Command(DockCommand::open_window(&a, Some(&m))));
            pop.popdown();
        }
    });
    // Title and workspace are separate labels. Titles run long ("Inbox —
    // Bence Szluka — Outlook") and are ellipsised; the workspace is the part
    // that tells two windows of one app apart, so it must never be the part
    // that gets cut off.
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title_label = gtk::Label::new(Some(&title));
    title_label.set_xalign(0.0);
    title_label.set_hexpand(true);
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_label.set_max_width_chars(30);
    content.append(&title_label);
    if !meta.workspace.is_empty() {
        let ws = gtk::Label::new(Some(meta.workspace_label()));
        ws.add_css_class("dock-menu-workspace");
        content.append(&ws);
    }
    focus.set_child(Some(&content));
    focus.set_hexpand(true);
    line.append(&focus);

    // Minimize or restore in place, so a window can be put away without
    // leaving the menu for the icon.
    let (glyph, tip) = if meta.minimized {
        ("\u{f2d2}", "Restore") // fa-window-restore
    } else {
        ("\u{f2d1}", "Minimize") // fa-window-minimize
    };
    let toggle = gtk::Button::with_label(glyph);
    toggle.add_css_class("dock-menu-item");
    toggle.add_css_class("dock-glyph");
    toggle.set_has_frame(false);
    toggle.set_tooltip_text(Some(tip));
    {
        let (a, m, cb, pop) = (addr.clone(), meta.clone(), on_action.clone(), popover.clone());
        toggle.connect_clicked(move |_| {
            let cmd = if m.minimized {
                DockCommand::restore(&a, &m)
            } else {
                DockCommand::minimize(&a, &m)
            };
            cb(MenuAction::Command(cmd));
            pop.popdown();
        });
    }
    line.append(&toggle);

    // Moving a minimized window would leave its home tag behind; restoring
    // is the way out.
    if !meta.minimized {
        // The move targets live in a nested popover so the main menu stays one
        // row per window however many workspaces there are.
        let mover = gtk::Button::with_label("\u{f061}"); // arrow-right glyph
        mover.add_css_class("dock-menu-item");
        mover.add_css_class("dock-glyph");
        mover.set_has_frame(false);
        mover.set_tooltip_text(Some("Move to workspace"));
        {
            let a = addr.clone();
            let current = meta.workspace.clone();
            let cb = on_action.clone();
            let pop = popover.clone();
            mover.connect_clicked(move |btn| {
                let sub = move_menu(&a, &current, &cb, &pop);
                sub.set_parent(btn);
                sub.connect_closed(|p| p.unparent());
                sub.popup();
            });
        }
        line.append(&mover);
    }
    line
}

/// The nested "move to" popover: workspaces 1–9 and the scratchpad.
fn move_menu<F>(
    addr: &crate::hypr::Address,
    current: &str,
    on_action: &F,
    outer: &gtk::Popover,
) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let sub = gtk::Popover::new();
    sub.add_css_class("dock-menu");
    sub.set_autohide(true);
    sub.set_position(gtk::PositionType::Right);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");
    list.append(&heading("Move to workspace"));

    // A row of numbers, like the bar's workspace widget, rather than nine
    // menu rows.
    let grid = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    grid.add_css_class("dock-move-grid");
    for n in 1..=MOVE_TARGETS {
        let name = n.to_string();
        let b = gtk::Button::with_label(&name);
        b.add_css_class("dock-menu-item");
        b.add_css_class("dock-move-target");
        b.set_has_frame(false);
        // Moving a window to where it already is does nothing; say so.
        b.set_sensitive(name != current);
        let a = addr.clone();
        let cb = on_action.clone();
        let (s2, o2) = (sub.clone(), outer.clone());
        b.connect_clicked(move |_| {
            cb(MenuAction::Command(DockCommand::SendToWorkspace {
                window: a.clone(),
                workspace: name.clone(),
            }));
            s2.popdown();
            o2.popdown();
        });
        grid.append(&b);
    }
    list.append(&grid);
    list.append(&separator());

    let scratch = format!("special:{}", crate::state::SCRATCHPAD);
    let in_scratch = current == scratch;
    let stash = row("Send to scratchpad", {
        let a = addr.clone();
        let cb = on_action.clone();
        let (s2, o2) = (sub.clone(), outer.clone());
        move || {
            cb(MenuAction::Command(DockCommand::SendToWorkspace {
                window: a.clone(),
                workspace: scratch.clone(),
            }));
            s2.popdown();
            o2.popdown();
        }
    });
    stash.set_sensitive(!in_scratch);
    list.append(&stash);

    sub.set_child(Some(&list));
    sub
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
