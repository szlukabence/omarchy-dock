//! Dock settings, reached by right-clicking the launcher button.
//!
//! Every control writes `config.toml` and lets the file watcher apply the
//! change, so the panel, a hand-edited config, and `omarchy-dockctl` all take
//! exactly one path into the running dock. That costs a file round-trip per
//! change and removes any chance of the panel and the file disagreeing.

use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;

use crate::config::{Config, HideMode, Position};
use crate::ui::menu::MenuAction;

/// Build the settings popover.
pub fn build<F>(on_action: F) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let cfg = Config::load();

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("dock-menu-list");
    list.add_css_class("dock-settings");

    // ── Omarchy surfaces ────────────────────────────────────────────────────
    // The launcher already opens the Omarchy menu on a left click; a right
    // click is the natural place to reach the shell's other surfaces directly.
    // These raise the real thing over IPC rather than the dock drawing a
    // lookalike, so they stay correct as Omarchy changes.
    list.append(&heading("Omarchy"));
    for surface in crate::omarchy::Surface::all() {
        let act = on_action.clone();
        let pop = popover.clone();
        list.append(&row(surface.label(), move || {
            act(MenuAction::OpenSurface(surface));
            pop.popdown();
        }));
    }

    list.append(&separator());
    list.append(&heading("Dock"));

    // ── position ────────────────────────────────────────────────────────────
    let positions = ["Bottom", "Top", "Left", "Right"];
    let current = match cfg.dock.position {
        Position::Bottom => 0,
        Position::Top => 1,
        Position::Left => 2,
        Position::Right => 3,
    };
    let pos = gtk::DropDown::from_strings(&positions);
    pos.set_selected(current);
    pos.connect_selected_notify(move |d| {
        edit(|c| {
            c.dock.position = match d.selected() {
                1 => Position::Top,
                2 => Position::Left,
                3 => Position::Right,
                _ => Position::Bottom,
            }
        })
    });
    list.append(&field("Position", &pos));

    // ── auto-hide ───────────────────────────────────────────────────────────
    let modes = ["Never", "Intelligent", "Always"];
    let current = match cfg.autohide.mode {
        HideMode::Never => 0,
        HideMode::Intelligent => 1,
        HideMode::Always => 2,
    };
    let hide = gtk::DropDown::from_strings(&modes);
    hide.set_selected(current);
    hide.connect_selected_notify(move |d| {
        edit(|c| {
            c.autohide.mode = match d.selected() {
                0 => HideMode::Never,
                2 => HideMode::Always,
                _ => HideMode::Intelligent,
            }
        })
    });
    list.append(&field("Auto-hide", &hide));

    // ── icon size ───────────────────────────────────────────────────────────
    let size = gtk::SpinButton::with_range(24.0, 128.0, 2.0);
    size.set_value(cfg.dock.icon_size);
    // `value-changed` fires on every step; the watcher debounces the writes.
    size.connect_value_changed(|s| {
        let v = s.value();
        edit(move |c| c.dock.icon_size = v)
    });
    list.append(&field("Icon size", &size));

    // ── magnification ───────────────────────────────────────────────────────
    let zoom = gtk::SpinButton::with_range(1.0, 2.5, 0.05);
    zoom.set_digits(2);
    zoom.set_value(cfg.magnify.zoom);
    zoom.connect_value_changed(|s| {
        let v = s.value();
        // 1.0 means no growth, which reads as "off" more clearly than a
        // separate switch that can disagree with the number.
        edit(move |c| {
            c.magnify.zoom = v;
            c.magnify.enabled = v > 1.001;
        })
    });
    list.append(&field("Hover zoom", &zoom));

    // ── icon spacing ────────────────────────────────────────────────────────
    // 0 means "derive it from the zoom factor", which is the default and keeps
    // magnified icons from overlapping. Any other value pins it.
    let spacing = gtk::SpinButton::with_range(0.0, 64.0, 1.0);
    spacing.set_value(cfg.dock.spacing.unwrap_or(0.0));
    spacing.set_tooltip_text(Some("0 = automatic (derived from hover zoom)"));
    spacing.connect_value_changed(|s| {
        let v = s.value();
        edit(move |c| c.dock.spacing = if v <= 0.0 { None } else { Some(v) })
    });
    list.append(&field("Icon spacing", &spacing));

    list.append(&separator());

    // ── toggles ─────────────────────────────────────────────────────────────
    list.append(&toggle("Show running apps", cfg.items.show_running, |v| {
        edit(move |c| c.items.show_running = v)
    }));
    list.append(&toggle("Workspaces", cfg.workspaces.enabled, |v| {
        edit(move |c| c.workspaces.enabled = v)
    }));
    list.append(&toggle("Scratchpad", cfg.workspaces.scratchpad, |v| {
        edit(move |c| c.workspaces.scratchpad = v)
    }));
    // One switch per folder: they are independent shortcuts, so a single
    // "show folders" toggle would be an all-or-nothing blunt instrument.
    for (i, folder) in cfg.items.folders.iter().enumerate() {
        let name = if folder.name.is_empty() { "Folder".to_string() } else { folder.name.clone() };
        list.append(&toggle(&name, folder.enabled, move |v| {
            edit(move |c| {
                if let Some(f) = c.items.folders.get_mut(i) {
                    f.enabled = v;
                }
            })
        }));
    }
    list.append(&toggle("Show Trash", cfg.items.show_trash, |v| {
        edit(move |c| c.items.show_trash = v)
    }));
    list.append(&toggle("Reserve screen space", cfg.dock.reserve_space, |v| {
        edit(move |c| c.dock.reserve_space = v)
    }));

    list.append(&separator());

    // ── separators ──────────────────────────────────────────────────────────
    {
        let cb = on_action.clone();
        let pop = popover.clone();
        list.append(&row("Add separator", move || {
            edit(|c| {
                let sep = crate::state::SEPARATOR;
                // Appending puts it exactly where the automatic divider
                // already goes, so the two collapse and nothing appears to
                // happen. Insert it mid-list instead, where it is visible and
                // can be dragged into place.
                let mut at = c.items.pinned.len() / 2;
                // Never land next to an existing separator: adjacent dividers
                // collapse when rendered, so the click would look like a
                // no-op and invite the user to click again, piling up dead
                // entries in the config.
                let is_sep = |i: usize| {
                    c.items.pinned.get(i).is_some_and(|p: &String| p.trim() == sep)
                };
                while at < c.items.pinned.len()
                    && (is_sep(at) || (at > 0 && is_sep(at - 1)))
                {
                    at += 1;
                }
                if at > 0 && is_sep(at - 1) {
                    return;
                }
                c.items.pinned.insert(at.min(c.items.pinned.len()), sep.into());
            });
            let _ = &cb;
            pop.popdown();
        }));
    }

    {
        let pop = popover.clone();
        list.append(&row("Remove last separator", move || {
            edit(|c| {
                if let Some(i) =
                    c.items.pinned.iter().rposition(|p| p.trim() == crate::state::SEPARATOR)
                {
                    c.items.pinned.remove(i);
                }
            });
            pop.popdown();
        }));
    }

    list.append(&separator());

    {
        let pop = popover.clone();
        list.append(&row("Edit config file…", move || {
            let path = crate::config::config_path();
            // Hand off to the desktop's handler rather than guessing an editor.
            let _ = std::process::Command::new("xdg-open").arg(path).spawn();
            pop.popdown();
        }));
    }

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_propagate_natural_height(true);
    // Cap the height so a short screen still shows the whole dock.
    scroll.set_max_content_height(520);
    scroll.set_child(Some(&list));
    popover.set_child(Some(&scroll));
    popover
}

/// Load, mutate, and save the config. The watcher applies it.
fn edit<F: FnOnce(&mut Config)>(f: F) {
    let mut cfg = Config::load();
    f(&mut cfg);
    if let Err(e) = cfg.save() {
        tracing::error!(error = %e, "cannot save settings");
    }
}

fn field(label: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("dock-settings-row");
    let l = gtk::Label::new(Some(label));
    l.add_css_class("dock-menu-item");
    l.set_xalign(0.0);
    l.set_hexpand(true);
    row.append(&l);
    row.append(control);
    row
}

fn toggle<F: Fn(bool) + 'static>(label: &str, on: bool, changed: F) -> gtk::Box {
    let sw = gtk::Switch::new();
    sw.set_active(on);
    sw.set_valign(gtk::Align::Center);
    sw.connect_state_set(move |_, v| {
        changed(v);
        glib::Propagation::Proceed
    });
    field(label, &sw)
}

fn row<F: Fn() + 'static>(label: &str, on_click: F) -> gtk::Button {
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
