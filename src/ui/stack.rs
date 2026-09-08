//! Folder-stack and Trash popovers.
//!
//! A stack lists a directory's most recent entries with quick actions. Rows
//! carry their own reveal/delete buttons rather than a nested context menu:
//! the popover is already transient, and a second layer of menus inside a
//! layer-shell popover is fiddly to dismiss.

use gtk4 as gtk;

use gtk::prelude::*;

use crate::stacks::{self, StackEntry};
use std::path::Path;

/// Build the popover for a folder stack.
pub fn build_folder<R: Fn() + Clone + 'static>(
    dir: &Path,
    label: &str,
    on_change: R,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");
    list.add_css_class("dock-stack");

    let entries = stacks::recent(dir, stacks::STACK_LIMIT);
    list.append(&heading(&format!(
        "{label} — {}",
        match entries.len() {
            0 => "empty".to_string(),
            1 => "1 item".to_string(),
            n => format!("{n} recent"),
        }
    )));

    for entry in &entries {
        list.append(&file_row(entry, &popover, on_change.clone(), true));
    }

    list.append(&separator());
    {
        let dir = dir.to_path_buf();
        let pop = popover.clone();
        list.append(&action_row("Open folder", move || {
            stacks::open(&dir);
            pop.popdown();
        }));
    }

    popover.set_child(Some(&scroller(&list)));
    popover
}

/// Build the popover for Trash.
pub fn build_trash<R: Fn() + Clone + 'static>(on_change: R) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let dir = stacks::trash_files_dir();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("dock-menu-list");
    list.add_css_class("dock-stack");

    let entries = stacks::recent(&dir, stacks::STACK_LIMIT);
    list.append(&heading(if entries.is_empty() {
        "Trash — empty"
    } else {
        "Trash"
    }));

    for entry in &entries {
        // Restoring is not offered: it needs the .trashinfo original path, and
        // silently "restoring" to the wrong place is worse than not offering.
        list.append(&file_row(entry, &popover, on_change.clone(), false));
    }

    if !entries.is_empty() {
        list.append(&separator());
        let pop = popover.clone();
        let cb = on_change.clone();
        list.append(&action_row("Empty Trash", move || {
            let n = stacks::empty_trash();
            tracing::info!(removed = n, "trash emptied");
            cb();
            pop.popdown();
        }));
    }

    {
        let pop = popover.clone();
        list.append(&action_row("Open Trash folder", move || {
            stacks::open(&stacks::trash_files_dir());
            pop.popdown();
        }));
    }

    popover.set_child(Some(&scroller(&list)));
    popover
}

/// One file row: icon, name, and quick actions.
fn file_row<R: Fn() + Clone + 'static>(
    entry: &StackEntry,
    popover: &gtk::Popover,
    on_change: R,
    offer_trash: bool,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("dock-stack-row");

    let open = gtk::Button::new();
    open.add_css_class("dock-menu-item");
    open.set_has_frame(false);
    open.set_hexpand(true);

    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let icon = gtk::Image::from_icon_name(if entry.is_dir {
        "folder"
    } else {
        "text-x-generic"
    });
    icon.set_pixel_size(16);
    inner.append(&icon);

    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    // Long filenames must not stretch the popover arbitrarily wide.
    name.set_max_width_chars(28);
    inner.append(&name);
    open.set_child(Some(&inner));

    {
        let path = entry.path.clone();
        let pop = popover.clone();
        open.connect_clicked(move |_| {
            stacks::open(&path);
            pop.popdown();
        });
    }
    row.append(&open);

    row.append(&icon_button("folder-open-symbolic", "Reveal", {
        let path = entry.path.clone();
        let pop = popover.clone();
        move || {
            stacks::reveal(&path);
            pop.popdown();
        }
    }));

    if offer_trash {
        row.append(&icon_button("user-trash-symbolic", "Move to Trash", {
            let path = entry.path.clone();
            let pop = popover.clone();
            let cb = on_change.clone();
            move || {
                if stacks::trash(&path) {
                    cb();
                }
                pop.popdown();
            }
        }));
    } else {
        row.append(&icon_button("edit-delete-symbolic", "Delete permanently", {
            let path = entry.path.clone();
            let pop = popover.clone();
            let cb = on_change.clone();
            move || {
                let ok = if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(&path).is_ok()
                } else {
                    std::fs::remove_file(&path).is_ok()
                };
                if ok {
                    cb();
                }
                pop.popdown();
            }
        }));
    }

    row
}

fn icon_button<F: Fn() + 'static>(icon: &str, tip: &str, on_click: F) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("dock-stack-action");
    b.set_has_frame(false);
    b.set_tooltip_text(Some(tip));
    b.connect_clicked(move |_| on_click());
    b
}

fn action_row<F: Fn() + 'static>(label: &str, on_click: F) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.add_css_class("dock-menu-item");
    b.set_has_frame(false);
    if let Some(l) = b.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    b.connect_clicked(move |_| on_click());
    b
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

fn scroller(list: &gtk::Box) -> gtk::ScrolledWindow {
    let sc = gtk::ScrolledWindow::new();
    sc.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sc.set_propagate_natural_height(true);
    sc.set_max_content_height(420);
    sc.set_child(Some(list));
    sc
}

