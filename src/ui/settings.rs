//! Dock settings, reached by right-clicking the launcher button.
//!
//! Two surfaces. The right-click itself opens a short menu — the Omarchy
//! surfaces the dock can raise, plus the handful of actions that are one
//! click and done. Everything with a value attached lives in a proper window
//! behind "Dock settings…", because a popover that has to scroll is a popover
//! that has outgrown being one.
//!
//! Every control writes `config.toml` and lets the file watcher apply the
//! change, so the panel, a hand-edited config, and `omarchy-dockctl` all take
//! exactly one path into the running dock. That costs a file round-trip per
//! change and removes any chance of the panel and the file disagreeing.

use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;

use std::cell::RefCell;

use crate::config::{Config, HideMode, Hover, Position, Style};
use crate::ui::menu::MenuAction;

/// The launcher's right-click menu: shell surfaces and one-click actions.
///
/// Deliberately short. Anything with a value to choose lives in the settings
/// window instead — a menu you have to scroll is not a menu.
pub fn build<F>(on_action: F) -> gtk::Popover
where
    F: Fn(MenuAction) + Clone + 'static,
{
    let popover = gtk::Popover::new();
    popover.add_css_class("dock-menu");
    popover.set_autohide(true);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("dock-menu-list");

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

    {
        let pop = popover.clone();
        list.append(&row("Settings…", move || {
            pop.popdown();
            open_window();
        }));
    }

    {
        let pop = popover.clone();
        list.append(&row("Add separator", move || {
            add_separator();
            pop.popdown();
        }));
    }

    {
        let pop = popover.clone();
        list.append(&row("Edit config file…", move || {
            let path = crate::config::config_path();
            // Hand off to the desktop's handler rather than guessing an editor.
            let _ = std::process::Command::new("xdg-open").arg(path).spawn();
            pop.popdown();
        }));
    }

    popover.set_child(Some(&list));
    popover
}

/// Open the settings window, or raise the one already open.
///
/// A single instance: opening a second copy of a panel that edits one file on
/// disk would let the two disagree about what that file currently says.
pub fn open_window() {
    thread_local! {
        static OPEN: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
    }

    OPEN.with(|slot| {
        if let Some(win) = slot.borrow().as_ref() {
            win.present();
            return;
        }

        let win = build_window();
        {
            let slot_clear = || OPEN.with(|s| *s.borrow_mut() = None);
            win.connect_close_request(move |_| {
                slot_clear();
                glib::Propagation::Proceed
            });
        }
        win.present();
        *slot.borrow_mut() = Some(win);
    });
}

fn build_window() -> gtk::Window {
    let cfg = Config::load();

    let win = gtk::Window::builder()
        .title("Dock Settings")
        .default_width(420)
        .default_height(620)
        .resizable(true)
        .build();
    win.add_css_class("dock-settings-window");

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("dock-settings");
    list.set_margin_top(12);
    list.set_margin_bottom(12);
    list.set_margin_start(8);
    list.set_margin_end(8);

    // ── placement ───────────────────────────────────────────────────────────
    list.append(&heading("Placement"));

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

    list.append(&toggle("Reserve screen space", cfg.dock.reserve_space, |v| {
        edit(move |c| c.dock.reserve_space = v)
    }));

    list.append(&separator());

    // ── appearance ──────────────────────────────────────────────────────────
    list.append(&heading("Appearance"));

    let styles = ["Omarchy", "Glass"];
    let style = gtk::DropDown::from_strings(&styles);
    style.set_selected(if cfg.theme.style == Style::Glass { 1 } else { 0 });
    style.connect_selected_notify(move |d| {
        let glass = d.selected() == 1;
        edit(move |c| c.theme.style = if glass { Style::Glass } else { Style::Omarchy })
    });
    style.set_tooltip_text(Some(
        "Omarchy: the theme's own tokens, opaque. Glass: translucent and blurred.",
    ));
    list.append(&field("Style", &style));

    let icon = gtk::SpinButton::with_range(16.0, 128.0, 2.0);
    icon.set_value(cfg.dock.icon_size);
    icon.connect_value_changed(|s| {
        let v = s.value();
        edit(move |c| c.dock.icon_size = v)
    });
    list.append(&field("Icon size", &icon));

    // 0 means "derive it from the zoom factor", which is the default and keeps
    // magnified icons from overlapping. Any other value pins it.
    let spacing = gtk::SpinButton::with_range(0.0, 64.0, 1.0);
    spacing.set_value(cfg.dock.spacing.unwrap_or(0.0));
    spacing.set_tooltip_text(Some("0 = automatic"));
    spacing.connect_value_changed(|s| {
        let v = s.value();
        edit(move |c| c.dock.spacing = if v <= 0.0 { None } else { Some(v) })
    });
    list.append(&field("Icon spacing", &spacing));

    list.append(&toggle("Monochrome glyphs", cfg.items.glyph_ui, |v| {
        edit(move |c| c.items.glyph_ui = v)
    }));

    list.append(&separator());

    // ── hover ───────────────────────────────────────────────────────────────
    list.append(&heading("Hover"));

    // Zoom is only meaningful in one of the three modes, so the control that
    // sets it follows the one that selects them rather than sitting there
    // looking live when it does nothing.
    let hovers = ["Highlight", "Magnify", "Nothing"];
    let hover = gtk::DropDown::from_strings(&hovers);
    hover.set_selected(match cfg.magnify.hover {
        Hover::Fill => 0,
        Hover::Scale => 1,
        Hover::None => 2,
    });
    hover.set_tooltip_text(Some(
        "Highlight matches the rest of Omarchy; magnify is the classic dock effect.",
    ));

    let zoom = gtk::SpinButton::with_range(1.05, 2.5, 0.05);
    zoom.set_digits(2);
    zoom.set_value(cfg.magnify.zoom.max(1.05));
    zoom.connect_value_changed(|s| {
        let v = s.value();
        edit(move |c| c.magnify.zoom = v)
    });
    let zoom_row = field("Magnification", &zoom);
    zoom_row.set_sensitive(cfg.magnify.hover == Hover::Scale);

    {
        let zoom_row = zoom_row.clone();
        hover.connect_selected_notify(move |d| {
            let mode = match d.selected() {
                1 => Hover::Scale,
                2 => Hover::None,
                _ => Hover::Fill,
            };
            zoom_row.set_sensitive(mode == Hover::Scale);
            edit(move |c| {
                c.magnify.hover = mode;
                // `enabled` gates hover reactions as a whole; the mode says
                // which one. Keeping them in step means neither can silently
                // cancel the other.
                c.magnify.enabled = mode != Hover::None;
            })
        });
    }
    list.append(&field("On hover", &hover));
    list.append(&zoom_row);

    let delay = gtk::SpinButton::with_range(0.0, 2000.0, 50.0);
    delay.set_value(cfg.dock.tooltip_delay_ms as f64);
    delay.connect_value_changed(|s| {
        let v = s.value() as u64;
        edit(move |c| c.dock.tooltip_delay_ms = v)
    });
    list.append(&field("Name delay (ms)", &delay));

    list.append(&separator());

    // ── items ───────────────────────────────────────────────────────────────
    list.append(&heading("Items"));

    list.append(&toggle("Show running apps", cfg.items.show_running, |v| {
        edit(move |c| c.items.show_running = v)
    }));
    list.append(&toggle("Workspaces", cfg.workspaces.enabled, |v| {
        edit(move |c| c.workspaces.enabled = v)
    }));
    list.append(&toggle("Empty workspaces", cfg.workspaces.show_empty, |v| {
        edit(move |c| c.workspaces.show_empty = v)
    }));
    list.append(&toggle("Scratchpad", cfg.workspaces.scratchpad, |v| {
        edit(move |c| c.workspaces.scratchpad = v)
    }));
    list.append(&toggle("System tray", cfg.tray.enabled, |v| {
        edit(move |c| c.tray.enabled = v)
    }));
    // Hosting the tray means claiming a bus name and registering with the
    // watcher, which happens once at startup.
    list.append(&note("The tray needs a dock restart to start or stop hosting."));

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

    list.append(&separator());

    // ── separators ──────────────────────────────────────────────────────────
    list.append(&heading("Separators"));
    list.append(&row("Add separator", add_separator));
    list.append(&row("Remove last separator", || {
        edit(|c| {
            if let Some(i) =
                c.items.pinned.iter().rposition(|p| p.trim() == crate::state::SEPARATOR)
            {
                c.items.pinned.remove(i);
            }
        });
    }));
    list.append(&note("Drag a separator on the dock to move it."));

    list.append(&separator());
    list.append(&row("Edit config file…", || {
        let path = crate::config::config_path();
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }));

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    win.set_child(Some(&scroll));
    win
}

/// Insert a divider somewhere it will actually be visible.
fn add_separator() {
    edit(|c| {
        let sep = crate::state::SEPARATOR;
        // Appending puts it exactly where the automatic divider already goes,
        // so the two collapse and nothing appears to happen. Insert it
        // mid-list instead, where it is visible and can be dragged into place.
        let mut at = c.items.pinned.len() / 2;
        // Never land next to an existing separator: adjacent dividers collapse
        // when rendered, so the click would look like a no-op and invite the
        // user to click again, piling up dead entries in the config.
        let is_sep =
            |i: usize| c.items.pinned.get(i).is_some_and(|p: &String| p.trim() == sep);
        while at < c.items.pinned.len() && (is_sep(at) || (at > 0 && is_sep(at - 1))) {
            at += 1;
        }
        if at > 0 && is_sep(at - 1) {
            return;
        }
        c.items.pinned.insert(at.min(c.items.pinned.len()), sep.into());
    });
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

/// A muted line of explanation under a control.
fn note(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dock-settings-note");
    l.set_xalign(0.0);
    l.set_wrap(true);
    l
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
