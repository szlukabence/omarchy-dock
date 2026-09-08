//! One dock surface: a layer-shell window carrying the glass panel and items.
//!
//! Magnification is a pure `gsk::Transform` — no relayout happens per frame,
//! so the compositor does the work and the frame cost stays flat as items are
//! added. Measured vsync-locked at 60Hz with idle cost of exactly zero,
//! because the tick callback uninstalls itself once every spring settles.

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::anim::Spring;
use crate::config::{Config, Position};
use crate::ui::Geometry;

/// Must match the Hyprland `layerrule` namespace.
pub const LAYER_NAMESPACE: &str = "omarchy-dock";

struct State {
    fixed: gtk::Fixed,
    items: Vec<gtk::Widget>,
    springs: Vec<Spring>,
    geom: Geometry,
    cfg: Config,
    hovered: Option<usize>,
    last_us: i64,
    ticking: bool,
}

impl State {
    /// Slot placement, then edge-anchored scale, as one GPU transform.
    ///
    /// `GtkFixed` expresses a child's position *as* its child transform, so
    /// `set_child_transform` replaces whatever `put()` established. Every
    /// transform must therefore re-apply the slot origin, or the item
    /// teleports to (0, 0) and vanishes.
    fn transform_for(&self, i: usize) -> gsk::Transform {
        let s = &self.springs[i];
        let (sx, sy) = self.geom.slots[i];
        let (ax, ay) = self.geom.anchor;
        let (lx, ly) = self.geom.lift_dir;

        let zoom = self.cfg.magnify.zoom;
        // 0..1 as the spring travels from rest to full zoom.
        let p = if zoom > 1.0 { ((s.pos - 1.0) / (zoom - 1.0)).clamp(0.0, 1.0) } else { 0.0 };
        let lift = self.cfg.magnify.lift * p;
        let k = s.pos as f32;

        gsk::Transform::new()
            .translate(&graphene::Point::new(sx as f32, sy as f32))
            .translate(&graphene::Point::new((lx * lift) as f32, (ly * lift) as f32))
            .translate(&graphene::Point::new(ax as f32, ay as f32))
            .scale(k, k)
            .translate(&graphene::Point::new(-ax as f32, -ay as f32))
    }

    fn apply(&self, i: usize) {
        self.fixed.set_child_transform(&self.items[i], Some(&self.transform_for(i)));
    }

    fn retarget(&mut self) {
        let zoom = if self.cfg.magnify.enabled { self.cfg.magnify.zoom } else { 1.0 };
        for (i, s) in self.springs.iter_mut().enumerate() {
            s.target = if Some(i) == self.hovered { zoom } else { 1.0 };
        }
    }
}

pub struct DockSurface {
    pub window: gtk::ApplicationWindow,
    /// Kept so later phases can update items in place instead of rebuilding.
    #[allow(dead_code)]
    state: Rc<RefCell<State>>,
}

impl DockSurface {
    pub fn build(
        app: &gtk::Application,
        cfg: &Config,
        monitor: Option<&gdk::Monitor>,
    ) -> Self {
        let ids = visible_items(cfg);
        let geom = Geometry::compute(cfg, ids.len());

        let fixed = gtk::Fixed::new();
        fixed.set_size_request(geom.window_w as i32, geom.window_h as i32);

        // Panel first so items draw over it.
        let panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        panel.add_css_class("dock-panel");
        panel.set_size_request(geom.panel_w as i32, geom.panel_h as i32);
        fixed.put(&panel, geom.panel_x, geom.panel_y);

        // Index desktop entries once. `AppInfo::all()` walks every installed
        // .desktop file, so doing it per item would be quadratic.
        let entries = desktop_index();

        let mut items = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            let img = make_icon(id, cfg.dock.icon_size as i32, &entries);
            img.set_size_request(cfg.dock.icon_size as i32, cfg.dock.icon_size as i32);
            img.set_tooltip_text(Some(id));
            let (x, y) = geom.slots[i];
            fixed.put(&img, x, y);
            items.push(img.upcast::<gtk::Widget>());
        }

        let state = Rc::new(RefCell::new(State {
            fixed: fixed.clone(),
            springs: vec![Spring::at(1.0); items.len()],
            items,
            geom,
            cfg: cfg.clone(),
            hovered: None,
            last_us: 0,
            ticking: false,
        }));

        if cfg.magnify.enabled {
            attach_motion(&fixed, &state, cfg.dock.icon_size);
        }

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .child(&fixed)
            .build();

        init_layer_shell(&window, cfg, monitor);
        window.present();

        Self { window, state }
    }

    pub fn close(&self) {
        self.window.close();
    }
}

/// Anchor the surface to the configured screen edge.
fn init_layer_shell(window: &gtk::ApplicationWindow, cfg: &Config, monitor: Option<&gdk::Monitor>) {
    window.init_layer_shell();
    window.set_namespace(Some(LAYER_NAMESPACE));
    window.set_layer(Layer::Top);

    if let Some(m) = monitor {
        window.set_monitor(Some(m));
    }

    let edge = match cfg.dock.position {
        Position::Bottom => Edge::Bottom,
        Position::Top => Edge::Top,
        Position::Left => Edge::Left,
        Position::Right => Edge::Right,
    };
    window.set_anchor(edge, true);
    window.set_margin(edge, cfg.dock.edge_offset);

    if cfg.dock.reserve_space {
        window.auto_exclusive_zone_enable();
    } else {
        // Float over windows without reserving screen space.
        window.set_exclusive_zone(0);
    }
}

/// Pinned apps, plus the trash slot when enabled.
///
/// Running-but-unpinned apps join this list in Phase 3, once Hyprland IPC and
/// desktop-entry matching exist.
fn visible_items(cfg: &Config) -> Vec<String> {
    let mut v = cfg.items.pinned.clone();
    if cfg.items.show_trash {
        v.push("user-trash".into());
    }
    v
}

/// Resolve an item id to an image.
///
/// Ids come from the user's pin list and may be desktop-entry ids, Hyprland
/// window classes, or plain icon names. Desktop entries win because they carry
/// the app's real icon; otherwise the id is tried as an icon name directly.
fn make_icon(id: &str, size: i32, entries: &HashMap<String, gio::AppInfo>) -> gtk::Image {
    let img = gtk::Image::new();
    img.set_pixel_size(size);
    img.add_css_class("dock-icon");

    if let Some(icon) = entries.get(&id.to_lowercase()).and_then(|e| e.icon()) {
        img.set_from_gicon(&icon);
        return img;
    }

    let has = gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d))
        .is_some_and(|t| t.has_icon(id));

    img.set_icon_name(Some(if has { id } else { "application-x-executable" }));
    img
}

/// Map desktop-entry id (lowercased, without the `.desktop` suffix) to its
/// `AppInfo`.
///
/// gio-rs does not bind `GDesktopAppInfo` (it lives in gio-unix), so entries
/// are reached through `AppInfo::all()` instead. Phase 3 replaces this with a
/// cached matcher that also indexes `StartupWMClass` for Hyprland classes.
fn desktop_index() -> HashMap<String, gio::AppInfo> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|a| {
            let id = a.id()?;
            let key = id.strip_suffix(".desktop").unwrap_or(&id).to_lowercase();
            Some((key, a))
        })
        .collect()
}

// ── hover and animation ─────────────────────────────────────────────────────

fn attach_motion(fixed: &gtk::Fixed, state: &Rc<RefCell<State>>, icon: f64) {
    let motion = gtk::EventControllerMotion::new();
    {
        let state = state.clone();
        motion.connect_motion(move |_, x, y| {
            let hit = state.borrow().geom.slot_at(x, y, icon);
            set_hover(&state, hit);
        });
    }
    {
        let state = state.clone();
        motion.connect_leave(move |_| set_hover(&state, None));
    }
    fixed.add_controller(motion);
}

fn set_hover(state: &Rc<RefCell<State>>, hit: Option<usize>) {
    {
        let mut s = state.borrow_mut();
        if s.hovered == hit {
            return;
        }
        s.hovered = hit;
        s.retarget();
    }
    ensure_ticking(state);
}

/// Install a frame-clock callback if one is not already running.
///
/// It removes itself once every spring has settled, so a dock nobody is
/// pointing at does no per-frame work at all.
fn ensure_ticking(state: &Rc<RefCell<State>>) {
    {
        let mut s = state.borrow_mut();
        if s.ticking {
            return;
        }
        s.ticking = true;
        s.last_us = 0;
    }

    let widget = state.borrow().fixed.clone();
    let state = state.clone();

    widget.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut s = state.borrow_mut();

        // First frame only establishes the time base.
        if s.last_us == 0 {
            s.last_us = now;
            return glib::ControlFlow::Continue;
        }
        let dt = (now - s.last_us) as f64 / 1_000_000.0;
        s.last_us = now;

        let cfg = s.cfg.clone();
        let mut moving = false;
        for i in 0..s.springs.len() {
            if s.springs[i].settled() {
                continue;
            }
            s.springs[i].step_cfg(dt, &cfg);
            if s.springs[i].settled() {
                s.springs[i].settle();
            } else {
                moving = true;
            }
            s.apply(i);
        }

        if moving {
            glib::ControlFlow::Continue
        } else {
            s.ticking = false;
            glib::ControlFlow::Break
        }
    });
}
