//! One dock surface: a layer-shell window carrying the glass panel and items.
//!
//! Magnification is a pure `gsk::Transform` — no relayout happens per frame,
//! so the compositor does the work and the frame cost stays flat as items are
//! added. Measured vsync-locked at 60Hz with idle cost of exactly zero,
//! because the tick callback uninstalls itself once every spring settles.

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::anim::Spring;
use crate::config::{Config, Hover, Position};
use crate::runtime::DockCommand;
use crate::state::{DockItem, ItemKind};
use crate::ui::menu::{self, MenuAction};
use crate::ui::Geometry;

/// Actions a dock surface emits. The app owns the worker channels and config,
/// so the UI reports intent rather than acting on it.
pub type ActionSink = Rc<dyn Fn(MenuAction)>;

/// Must match the Hyprland `layerrule` namespace.
pub const LAYER_NAMESPACE: &str = "omarchy-dock";

/// Bounce spring: deliberately underdamped so the icon overshoots and settles
/// with a couple of visible swings.
const BOUNCE_STIFFNESS: f64 = 220.0;
/// ~0.28 of critical damping (2*sqrt(220) ~= 29.66), precomputed because
/// `sqrt` is not const.
const BOUNCE_DAMPING: f64 = 8.3;
/// Initial upward velocity, in px/s, of a launch or urgency bounce.
const BOUNCE_IMPULSE: f64 = -320.0;

/// How far the hover plate extends past the icon box on each side. Small: the
/// shell's own hover fill hugs its content rather than framing it.
const PLATE_MARGIN: f64 = 4.0;

/// Spring for the hover fill. Critically damped (2*sqrt(1600) = 80) and stiff,
/// because a highlight that lags the pointer feels broken rather than smooth.
const HOVER_STIFFNESS: f64 = 1600.0;
const HOVER_DAMPING: f64 = 80.0;

/// Spring for the gap that opens at a drop position. Critically damped
/// (2*sqrt(900) = 60), precomputed because `sqrt` is not const.
const SHIFT_STIFFNESS: f64 = 900.0;
const SHIFT_DAMPING: f64 = 60.0;

struct State {
    fixed: gtk::Fixed,
    /// Current item data. Click handlers read this by index rather than
    /// capturing a copy, so an in-place refresh cannot leave them stale.
    data: Vec<DockItem>,
    /// Per-slot indicator and badge widgets. Always created, shown or hidden
    /// as state changes, so a refresh never has to build widgets.
    indicators: Vec<gtk::Widget>,
    badges: Vec<gtk::Label>,
    /// The hovered icon's name, drawn in the reserved band at the top of the
    /// surface. GTK's own tooltips follow the pointer, which puts the name
    /// below the icon and over the panel; a dock wants it above the icon.
    tip_label: gtk::Label,
    /// Bumped on every hover change so a late tooltip timer is discarded.
    tip_generation: u64,
    tooltip_delay: u64,
    items: Vec<gtk::Widget>,
    /// Per-slot plate drawn *behind* the icon, carrying the shell's hover
    /// fill. Behind rather than around it so a magnified icon grows over its
    /// own highlight instead of being clipped by it.
    plates: Vec<gtk::Widget>,
    /// Hover progress, 0..1, driving the plate's opacity. Separate from the
    /// zoom spring because the fill and the magnification are alternatives:
    /// in fill mode the zoom spring never leaves 1.0, so it carries no signal.
    hovers: Vec<Spring>,
    /// Where each slot's widget currently sits in these vectors.
    ///
    /// Every per-slot controller holds the cell belonging to its own widget
    /// rather than a plain index, because a reorder permutes the widgets while
    /// leaving their controllers attached. An index captured when the slot was
    /// built would then name whichever item had moved into that position —
    /// clicking one icon would launch another, and dragging one would move
    /// another. The cell travels with the widget, so it stays true.
    slot_index: Vec<Rc<Cell<usize>>>,
    springs: Vec<Spring>,
    /// Displacement away from the screen edge, in pixels, for launch bounce
    /// and urgency. Separate from the zoom spring so a bounce can play while
    /// the icon is magnified.
    bounces: Vec<Spring>,
    /// Sideways displacement along the dock's long axis, used to open a gap at
    /// the drop position while dragging. Its own spring so it composes with
    /// magnification and bounce rather than fighting them.
    shifts: Vec<Spring>,
    /// Rendered index the drop would insert before, while a drag is over the
    /// dock.
    drop_at: Option<usize>,
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

        let bounce = self.bounces[i].pos;
        let shift = self.shifts[i].pos;
        let zoom = self.cfg.magnify.zoom;
        // 0..1 as the spring travels from rest to full zoom.
        let p = if zoom > 1.0 { ((s.pos - 1.0) / (zoom - 1.0)).clamp(0.0, 1.0) } else { 0.0 };
        // Bounce rides on top of magnification lift, along the same axis.
        let lift = self.cfg.magnify.lift * p + bounce;
        let k = s.pos as f32;

        // The gap opens along the dock's long axis, which is the axis the
        // edge normal is *not* on.
        let (shift_x, shift_y) =
            if self.geom.horizontal() { (shift, 0.0) } else { (0.0, shift) };

        gsk::Transform::new()
            .translate(&graphene::Point::new(
                (sx + shift_x) as f32,
                (sy + shift_y) as f32,
            ))
            .translate(&graphene::Point::new((lx * lift) as f32, (ly * lift) as f32))
            .translate(&graphene::Point::new(ax as f32, ay as f32))
            .scale(k, k)
            .translate(&graphene::Point::new(-ax as f32, -ay as f32))
    }

    fn apply(&self, i: usize) {
        self.fixed.set_child_transform(&self.items[i], Some(&self.transform_for(i)));

        // The plate follows the slot along the dock's axis so it travels with
        // a drop gap, but it deliberately does not zoom, lift or bounce: it is
        // the seat the icon sits in, not part of the icon.
        if let Some(plate) = self.plates.get(i) {
            let (sx, sy) = self.geom.slots[i];
            let shift = self.shifts[i].pos;
            let (dx, dy) = if self.geom.horizontal() { (shift, 0.0) } else { (0.0, shift) };
            self.fixed.set_child_transform(
                plate,
                Some(&gsk::Transform::new().translate(&graphene::Point::new(
                    (sx + dx) as f32,
                    (sy + dy) as f32,
                ))),
            );
            plate.set_opacity(self.hovers[i].pos.clamp(0.0, 1.0));
        }
    }

    fn retarget(&mut self) {
        let mode = if self.cfg.magnify.enabled { self.cfg.magnify.hover } else { Hover::None };
        let zoom = if mode == Hover::Scale { self.cfg.magnify.zoom } else { 1.0 };

        for i in 0..self.springs.len() {
            // A divider that swells or lights up on hover reads as a glitch,
            // so only real items react — but hover still registers, so
            // right-click works everywhere.
            let reacts = self.data.get(i).is_some_and(|d| d.kind != ItemKind::Separator);
            let on = Some(i) == self.hovered && reacts;
            self.springs[i].target = if on { zoom } else { 1.0 };
            self.hovers[i].target = if on && mode == Hover::Fill { 1.0 } else { 0.0 };
        }
    }
}

pub struct DockSurface {
    pub window: gtk::ApplicationWindow,
    /// Connector name (e.g. "eDP-1"), matching Hyprland's monitor name.
    pub monitor_name: Option<String>,
    /// Logical size of the visible glass panel — not the surface, which also
    /// carries magnification headroom and the edge offset as transparent
    /// padding. Auto-hide overlap must be tested against the panel.
    pub panel_size: (f64, f64),
    /// Slide offset animation, in pixels away from the screen edge.
    slide: Rc<RefCell<Slide>>,
    /// Kept so later phases can update items in place instead of rebuilding.
    #[allow(dead_code)]
    state: Rc<RefCell<State>>,
}

impl DockSurface {
    pub fn build(
        app: &gtk::Application,
        cfg: &Config,
        items: &[DockItem],
        monitor: Option<&gdk::Monitor>,
        sink: ActionSink,
    ) -> Self {
        let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
        let geom = Geometry::compute(cfg, &kinds);
        let travel = travel_for(cfg, &geom);
        let geom_size = (geom.panel_w, geom.panel_h);

        let fixed = gtk::Fixed::new();
        fixed.set_size_request(geom.window_w as i32, geom.window_h as i32);

        // Panel first so items draw over it.
        let panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        panel.add_css_class("dock-panel");
        panel.set_size_request(geom.panel_w as i32, geom.panel_h as i32);
        fixed.put(&panel, geom.panel_x, geom.panel_y);

        let size = cfg.dock.icon_size as i32;
        let mut widgets = Vec::with_capacity(items.len());
        let mut slots = Vec::with_capacity(items.len());
        let mut indicators = Vec::with_capacity(items.len());
        let mut badges = Vec::with_capacity(items.len());
        let mut plates = Vec::with_capacity(items.len());

        for (i, item) in items.iter().enumerate() {
            // Icon and badge share one widget so the badge tracks the icon as
            // it magnifies.
            let slot = gtk::Overlay::new();

            if item.kind == ItemKind::Separator {
                // A divider is inert: no magnification, no input, no
                // indicator. It only needs to occupy its slot.
                let rule = gtk::Box::new(gtk::Orientation::Vertical, 0);
                rule.add_css_class("dock-separator");
                rule.set_halign(gtk::Align::Center);
                rule.set_valign(gtk::Align::Center);
                if geom.horizontal() {
                    rule.set_size_request(2, (cfg.dock.icon_size * 0.58) as i32);
                } else {
                    rule.set_size_request((cfg.dock.icon_size * 0.58) as i32, 2);
                }
                let (ex, ey) = if geom.horizontal() {
                    (geom.extents[i], cfg.dock.icon_size)
                } else {
                    (cfg.dock.icon_size, geom.extents[i])
                };
                slot.set_size_request(ex as i32, ey as i32);
                slot.set_child(Some(&rule));

                let (x, y) = geom.slots[i];
                fixed.put(&slot, x, y);
                slots.push(slot.clone());
                widgets.push(slot.upcast::<gtk::Widget>());
                // Keep the per-slot vectors aligned with the item list. A
                // divider has no hover state, so its plate is never shown.
                plates.push(hover_plate(&fixed, x, y, 0.0, 0.0));
                let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                dot.set_visible(false);
                indicators.push(dot.upcast::<gtk::Widget>());
                let badge = gtk::Label::new(None);
                badge.set_visible(false);
                badges.push(badge);
                continue;
            }

            slot.set_size_request(size, size);

            // The plate goes in first so it draws beneath this slot. Sized a
            // little tighter than the icon box, the way a bar widget's
            // highlight sits inside its cell rather than filling it.
            let (px, py) = geom.slots[i];
            let plate_size = cfg.dock.icon_size + PLATE_MARGIN * 2.0;
            plates.push(hover_plate(
                &fixed,
                px - PLATE_MARGIN,
                py - PLATE_MARGIN,
                plate_size,
                plate_size,
            ));

            slot.set_child(Some(&item_visual(item, size, cfg)));

            let badge = gtk::Label::new(None);
            badge.add_css_class("dock-badge");
            badge.set_halign(gtk::Align::End);
            badge.set_valign(gtk::Align::Start);
            // Must be set here, not left to the first refresh: the badge has a
            // coloured background and rounded corners, so an empty *visible*
            // one renders as a stray dot on every icon until something else
            // triggers a refresh.
            match item.badge() {
                Some(n) => badge.set_text(&n.to_string()),
                None => badge.set_visible(false),
            }
            slot.add_overlay(&badge);
            badges.push(badge);

            let (x, y) = geom.slots[i];
            fixed.put(&slot, x, y);
            slots.push(slot.clone());
            widgets.push(slot.upcast::<gtk::Widget>());

            // The indicator is a separate, untransformed child: on macOS the
            // running dot stays put while the icon above it grows.
            let (len, thick) = if geom.horizontal() { (6.0, 3.0) } else { (3.0, 6.0) };
            let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            dot.add_css_class("dock-indicator");
            dot.set_size_request(len as i32, thick as i32);
            if let Some((ix, iy)) = geom.indicator_at(i, cfg.dock.icon_size, len, thick) {
                fixed.put(&dot, ix, iy);
            }
            indicators.push(dot.upcast::<gtk::Widget>());
        }
        let widget_count = widgets.len();

        // Name label lives in the reserved band at the top of the surface, so
        // it is never clipped and never takes input.
        let tip_label = gtk::Label::new(None);
        tip_label.add_css_class("dock-tip-label");
        tip_label.set_can_target(false);
        tip_label.set_visible(false);
        fixed.put(&tip_label, 0.0, 2.0);

        let slot_index: Vec<Rc<Cell<usize>>> =
            (0..widget_count).map(|i| Rc::new(Cell::new(i))).collect();

        let state = Rc::new(RefCell::new(State {
            fixed: fixed.clone(),
            springs: vec![Spring::at(1.0); widget_count],
            bounces: vec![Spring::at(0.0); widget_count],
            shifts: vec![Spring::at(0.0); widget_count],
            drop_at: None,
            data: items.to_vec(),
            indicators,
            badges,
            tip_label: tip_label.clone(),
            tip_generation: 0,
            tooltip_delay: cfg.dock.tooltip_delay_ms,
            items: widgets,
            plates,
            hovers: vec![Spring::at(0.0); widget_count],
            slot_index: slot_index.clone(),
            geom,
            cfg: cfg.clone(),
            hovered: None,
            last_us: 0,
            ticking: false,
        }));

        // Always: hover drives the name label and the shell's hover fill, not
        // only magnification.
        attach_motion(&fixed, &state, cfg.dock.icon_size);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .child(&fixed)
            .build();

        let edge = init_layer_shell(&window, cfg, monitor);
        window.present();

        let slide = Rc::new(RefCell::new(Slide {
            spring: Spring::at(0.0),
            hidden: false,
            peeking: false,
            held: 0,
            generation: 0,
            edge,
            // Zero: the edge offset lives inside the surface now, so the
            // surface itself is flush with the screen edge.
            base_margin: 0,
            // How far the surface must travel to be off-screen, minus the
            // sliver left behind as a pointer trigger.
            travel,
            ticking: false,
            last_us: 0,
        }));

        let surface = Self {
            window: window.clone(),
            monitor_name: monitor.and_then(|m| m.connector()).map(|c| c.to_string()),
            panel_size: geom_size,
            slide: slide.clone(),
            state: state.clone(),
        };

        // Clicks are wired after State exists so a launch can bounce its own
        // icon without a second lookup.
        for (i, slot) in slots.iter().enumerate() {
            let at = &slot_index[i];
            // Separators get clicks too: not to launch anything, but so their
            // right-click menu can remove them.
            attach_clicks(slot, &sink, &state, at, &slide, &window, cfg);
            if let Some(item) = items.get(i) {
                attach_drag(slot, item, &state, at, cfg, &slide, &window);
            }
        }
        attach_drop(&fixed, &state, &sink, cfg);

        // Test hook: drags cannot be synthesised against a layer surface, so
        // this forces the drop gap open to verify it parts correctly.
        if let Ok(n) = std::env::var("OMARCHY_DOCK_FORCE_GAP") {
            if let Ok(i) = n.parse::<usize>() {
                let st = state.clone();
                let icon = cfg.dock.icon_size;
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(400),
                    move || set_drop_gap(&st, Some(i), icon),
                );
            }
        }

        // Test hook: right-click cannot be synthesised against a layer surface
        // either, so this opens the launcher's settings popover on the given
        // slot to verify it renders.
        if let Ok(n) = std::env::var("OMARCHY_DOCK_FORCE_MENU") {
            if let Ok(i) = n.parse::<usize>() {
                let sink = sink.clone();
                let anchor = slots.get(i).cloned();
                let side = popover_side(cfg);
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(600),
                    move || {
                        let Some(anchor) = anchor else { return };
                        let pop = crate::ui::settings::build(move |a| sink(a));
                        pop.set_parent(&anchor);
                        pop.set_position(side);
                        pop.popup();
                    },
                );
            }
        }

        // Test hook: pointer input cannot be synthesised against a layer
        // surface, so this forces a hover to verify magnification and the name
        // label without a real pointer.
        if let Ok(n) = std::env::var("OMARCHY_DOCK_FORCE_HOVER") {
            if let Ok(i) = n.parse::<usize>() {
                let st = state.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(400),
                    move || set_hover(&st, Some(i)),
                );
            }
        }

        surface.attach_peek(cfg);

        // A rebuild replaces the surface, and the new one never receives an
        // `enter` for a pointer that was already inside it — so after a drop
        // or a config change the dock would decide the pointer had left and
        // hide out from under the cursor. Ask the compositor where the pointer
        // actually is instead of waiting to be told.
        surface.sync_peek_to_pointer(cfg);

        // Anything already demanding attention should bounce on appear.
        for (i, item) in items.iter().enumerate() {
            if item.urgent {
                surface.bounce(i);
            }
        }
        surface
    }

    pub fn close(&self) {
        self.window.close();
    }

    /// Refresh indicators, badges and tooltips without rebuilding.
    ///
    /// Returns false when the item *set* changed (different apps, or a
    /// different order), which needs new widgets. Rebuilding on every focus
    /// change would destroy and recreate the layer surface — losing slide
    /// state and flickering — so only shape changes pay that cost.
    pub fn refresh(&self, items: &[DockItem]) -> bool {
        let mut s = self.state.borrow_mut();
        if s.data.len() != items.len()
            || !s.data.iter().zip(items).all(|(a, b)| a.key == b.key)
        {
            return false;
        }

        for (i, item) in items.iter().enumerate() {
            if let Some(dot) = s.indicators.get(i) {
                dot.set_visible(item.running());
                // Toggle rather than add: classes persist across refreshes.
                set_class(dot, "urgent", item.urgent);
                set_class(dot, "active", item.active);
            }
            if let Some(badge) = s.badges.get(i) {
                match item.badge() {
                    Some(n) => {
                        badge.set_text(&n.to_string());
                        badge.set_visible(true);
                    }
                    None => badge.set_visible(false),
                }
            }

        }

        s.data = items.to_vec();
        true
    }

    /// Reorder existing widgets to match `items`, without rebuilding.
    ///
    /// Returns false when the item *set* differs, which genuinely needs new
    /// widgets. Rebuilding destroys and recreates the layer surface, which
    /// flickers and drops the dock for a frame — very visible when it happens
    /// on every drag-and-drop.
    pub fn reorder(&self, items: &[DockItem], cfg: &Config) -> bool {
        let mut s = self.state.borrow_mut();

        // Where each new position's widget currently sits.
        let Some(from) = crate::state::match_permutation(&s.data, items) else {
            return false;
        };

        // Permute every per-slot vector together, so springs, widgets and the
        // index cells their controllers read stay matched to their items.
        let permute = |v: &mut Vec<gtk::Widget>| {
            *v = from.iter().map(|&i| v[i].clone()).collect();
        };
        permute(&mut s.items);
        permute(&mut s.indicators);
        permute(&mut s.plates);
        s.badges = from.iter().map(|&i| s.badges[i].clone()).collect();
        s.springs = from.iter().map(|&i| s.springs[i]).collect();
        s.bounces = from.iter().map(|&i| s.bounces[i]).collect();
        s.shifts = from.iter().map(|&i| s.shifts[i]).collect();
        s.hovers = from.iter().map(|&i| s.hovers[i]).collect();
        s.slot_index = from.iter().map(|&i| s.slot_index[i].clone()).collect();
        s.data = items.to_vec();

        // Each widget's controllers resolve their item through this cell, so
        // it has to name where the widget landed. Miss this and clicking or
        // dragging a moved icon acts on whatever took its old place.
        for (i, cell) in s.slot_index.iter().enumerate() {
            cell.set(i);
        }
        tracing::debug!(
            ?from,
            keys = ?s.data.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(),
            "reordered in place"
        );

        // Kinds may have moved, so slot extents change with them.
        let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
        s.geom = Geometry::compute(cfg, &kinds);

        // Re-place everything. Position lives in the child transform, so this
        // is the same call that drives magnification.
        for i in 0..s.items.len() {
            s.apply(i);
        }
        for (i, item) in items.iter().enumerate() {
            if let Some((ix, iy)) = indicator_origin(&s.geom, i, cfg) {
                if let Some(dot) = s.indicators.get(i) {
                    s.fixed.move_(dot, ix, iy);
                    dot.set_visible(item.running());
                    set_class(dot, "urgent", item.urgent);
                    set_class(dot, "active", item.active);
                }
            }
        }

        // Hover indices refer to the old order; drop them rather than leave a
        // stale icon magnified.
        s.hovered = None;
        s.drop_at = None;
        for sp in s.springs.iter_mut() {
            sp.target = 1.0;
        }
        for sh in s.shifts.iter_mut() {
            sh.target = 0.0;
        }
        for h in s.hovers.iter_mut() {
            h.target = 0.0;
        }
        drop(s);
        ensure_ticking(&self.state);
        true
    }

    /// Slide the surface off-screen, or back on.
    ///
    /// A few pixels are deliberately left on screen: that sliver still
    /// receives pointer events, so the dock can reveal itself on hover without
    /// a separate trigger surface.
    pub fn set_hidden(&self, hidden: bool, cfg: &Config) {
        {
            let mut s = self.slide.borrow_mut();
            if s.hidden == hidden {
                return;
            }
            s.hidden = hidden;
        }
        self.apply_slide(cfg);
    }

    /// Drive the spring toward wherever policy and peek state agree it goes.
    fn apply_slide(&self, cfg: &Config) {
        {
            let mut s = self.slide.borrow_mut();
            let target = if s.hidden && !s.peeking && s.held == 0 { s.travel } else { 0.0 };
            tracing::debug!(
                target, hidden = s.hidden, peeking = s.peeking,
                current = s.spring.target, travel = s.travel, "policy retarget"
            );
            if (s.spring.target - target).abs() < f64::EPSILON {
                return;
            }
            s.spring.target = target;
        }
        self.animate_slide(cfg);
    }

    /// Whether the surface is currently slid away. Used by Phase 7's
    /// drag-to-reveal.
    #[allow(dead_code)]
    pub fn hidden(&self) -> bool {
        self.slide.borrow().hidden
    }

    /// Seed peek state from where the pointer actually is right now.
    fn sync_peek_to_pointer(&self, cfg: &Config) {
        if pointer_inside(&self.window) {
            surface_set_peeking(&self.slide, &self.window, cfg, true);
        }
    }

    /// Reveal on hover and re-hide after the pointer leaves.
    ///
    /// When hidden, the surface keeps a sliver on screen; that sliver still
    /// receives pointer events, which is what makes this work without a
    /// separate trigger surface.
    fn attach_peek(&self, cfg: &Config) {
        let motion = gtk::EventControllerMotion::new();
        let reveal_ms = cfg.autohide.reveal_delay_ms;
        let hide_ms = cfg.autohide.hide_delay_ms;

        {
            let slide = self.slide.clone();
            let window = self.window.clone();
            let cfg = cfg.clone();
            motion.connect_enter(move |_, _, _| {
                let gen = {
                    let mut s = slide.borrow_mut();
                    s.generation += 1;
                    s.generation
                };
                let (slide, window, cfg) = (slide.clone(), window.clone(), cfg.clone());
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(reveal_ms),
                    move || {
                        // Ignore a timer whose pointer has since moved on.
                        if slide.borrow().generation != gen {
                            return;
                        }
                        surface_set_peeking(&slide, &window, &cfg, true);
                    },
                );
            });
        }
        {
            let slide = self.slide.clone();
            let window = self.window.clone();
            let cfg = cfg.clone();
            motion.connect_leave(move |_| {
                let gen = {
                    let mut s = slide.borrow_mut();
                    s.generation += 1;
                    s.generation
                };
                let (slide, window, cfg) = (slide.clone(), window.clone(), cfg.clone());
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(hide_ms),
                    move || {
                        let s = slide.borrow();
                        if s.generation != gen {
                            return;
                        }
                        // A drag or open menu grabs the pointer and produces a
                        // `leave` that did not happen. Releasing the hold
                        // re-checks the real pointer position, so ignore this.
                        if s.held > 0 {
                            return;
                        }
                        drop(s);
                        surface_set_peeking(&slide, &window, &cfg, false);
                    },
                );
            });
        }
        self.window.add_controller(motion);
    }

    /// Drive the slide with the frame clock, applying it as a layer-shell
    /// margin. Unlike icon magnification this cannot be a GPU transform: the
    /// surface itself has to move, or it keeps eating input where it is no
    /// longer drawn.
    fn animate_slide(&self, cfg: &Config) {
        animate_slide_on(&self.slide, &self.window, cfg);
    }

    /// Kick an item upwards; used for launches and urgency.
    pub fn bounce(&self, i: usize) {
        kick(&self.state, i);
    }
}

/// Wire left-click (focus / cycle / launch) and right-click (menu).
#[allow(clippy::too_many_arguments)]
fn attach_clicks(
    slot: &gtk::Overlay,
    sink: &ActionSink,
    state: &Rc<RefCell<State>>,
    at: &Rc<Cell<usize>>,
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
) {
    // ── left button ─────────────────────────────────────────────────────────
    let left = gtk::GestureClick::new();
    left.set_button(gdk::BUTTON_PRIMARY);
    {
        let sink = sink.clone();
        let state = state.clone();
        let anchor_left = slot.clone();
        let slide_l = slide.clone();
        let window_l = window.clone();
        let cfg_l = cfg.clone();
        let menu_side = popover_side(cfg);
        let at = at.clone();
        left.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);

            // Read current data by the slot's live index: focus may have moved,
            // and a reorder may have moved this widget, since the dock was
            // built.
            let index = at.get();
            let item = {
                let s = state.borrow();
                match s.data.get(index) {
                    Some(i) => i.clone(),
                    None => return,
                }
            };

            match item.kind {
                ItemKind::Launcher => {
                    if !item.exec.is_empty() {
                        sink(MenuAction::Command(DockCommand::Exec(item.exec.clone())));
                    }
                    return;
                }
                // A separator has nothing to activate.
                ItemKind::Separator => return,
                // Stacks and Trash open a popover rather than launching.
                ItemKind::Folder | ItemKind::Trash => {
                    let sink2 = sink.clone();
                    let refresh = move || sink2(MenuAction::Rescan);
                    let pop = if item.kind == ItemKind::Trash {
                        crate::ui::stack::build_trash(refresh)
                    } else {
                        let Some(dir) = item.path.clone() else { return };
                        crate::ui::stack::build_folder(&dir, &item.label, refresh)
                    };
                    pop.set_parent(&anchor_left);
                    pop.set_position(menu_side);
                    hold_for_popover(&pop, &slide_l, &window_l, &cfg_l);
                    pop.popup();
                    return;
                }
                _ => {}
            }

            let action = if item.windows.is_empty() {
                // Nothing running: launch, unless this is a pure UI slot.
                (!item.exec.is_empty()).then(|| {
                    kick(&state, index);
                    MenuAction::Command(DockCommand::Exec(item.exec.clone()))
                })
            } else {
                // Running: focus, or cycle when this app already has focus.
                item.click_target()
                    .map(|next| MenuAction::Command(DockCommand::Focus(next.clone())))
            };

            if let Some(a) = action {
                sink(a);
            }
        });
    }
    slot.add_controller(left);

    // ── right button ────────────────────────────────────────────────────────
    let right = gtk::GestureClick::new();
    right.set_button(gdk::BUTTON_SECONDARY);
    {
        let sink = sink.clone();
        let anchor = slot.clone();
        let state = state.clone();
        let slide_r = slide.clone();
        let window_r = window.clone();
        let cfg_r = cfg.clone();
        let menu_side_r = popover_side(cfg);
        let at = at.clone();
        right.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let item = {
                let s = state.borrow();
                match s.data.get(at.get()) {
                    Some(i) => i.clone(),
                    None => return,
                }
            };
            let sink = sink.clone();
            let popover = if item.kind == ItemKind::Separator {
                // Only user-placed separators are editable; automatic dividers
                // are derived from the item list and have no pinned index.
                match item.pin_index {
                    Some(pin) => menu::build_separator(pin, move |a| sink(a)),
                    None => return,
                }
            } else if item.kind == ItemKind::Trash || item.kind == ItemKind::Folder {
                let sink2 = sink.clone();
                let refresh = move || sink2(MenuAction::Rescan);
                if item.kind == ItemKind::Trash {
                    crate::ui::stack::build_trash(refresh)
                } else {
                    match item.path.clone() {
                        Some(dir) => crate::ui::stack::build_folder(&dir, &item.label, refresh),
                        None => return,
                    }
                }
            } else if item.kind == ItemKind::Launcher {
                // The launcher has no windows or desktop actions, so its
                // right-click is the natural home for the dock's own settings.
                crate::ui::settings::build(move |a| sink(a))
            } else {
                menu::build(&item, move |a| sink(a))
            };
            popover.set_parent(&anchor);
            popover.set_position(menu_side_r);
            // hold_for_popover also unparents on close, so repeated
            // right-clicks do not stack popovers as children of the slot.
            hold_for_popover(&popover, &slide_r, &window_r, &cfg_r);
            popover.popup();
        });
    }
    slot.add_controller(right);
}

fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

/// Set peek state and re-drive the slide. Free-standing so the hover timers
/// can act without holding a `DockSurface`.
fn surface_set_peeking(
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
    peeking: bool,
) {
    {
        let mut s = slide.borrow_mut();
        if s.peeking == peeking {
            return;
        }
        s.peeking = peeking;
        let target = if s.hidden && !s.peeking && s.held == 0 { s.travel } else { 0.0 };
        if (s.spring.target - target).abs() < f64::EPSILON {
            return;
        }
        tracing::debug!(target, peeking, hidden = s.hidden, "peek retarget");
        s.spring.target = target;
    }
    animate_slide_on(slide, window, cfg);
}

/// Drive the slide with the frame clock, applying it as a layer-shell margin.
///
/// Unlike icon magnification this cannot be a GPU transform: the surface
/// itself has to move, or it keeps eating input where it is no longer drawn.
fn animate_slide_on(
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
) {
    {
        let mut s = slide.borrow_mut();
        if s.ticking {
            return;
        }
        s.ticking = true;
        s.last_us = 0;
    }

    let slide = slide.clone();
    let win = window.clone();
    // Critically damped: a dock sliding back should settle, not wobble.
    let stiffness = 1000.0 / (cfg.autohide.slide_ms.max(40) as f64 / 100.0);
    let damping = 2.0 * stiffness.sqrt();

    window.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut s = slide.borrow_mut();

        if s.last_us == 0 {
            s.last_us = now;
            return glib::ControlFlow::Continue;
        }
        let dt = (now - s.last_us) as f64 / 1_000_000.0;
        s.last_us = now;

        s.spring.step(dt, stiffness, damping);
        let settled = s.spring.settled();
        if settled {
            s.spring.settle();
        }

        let margin = s.base_margin - s.spring.pos.round() as i32;
        win.set_margin(s.edge, margin);

        if settled {
            tracing::debug!(
                pos = s.spring.pos, target = s.spring.target, margin,
                hidden = s.hidden, peeking = s.peeking, "slide settled"
            );
            s.ticking = false;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Slide state for one surface.
struct Slide {
    spring: Spring,
    /// What the auto-hide policy wants.
    hidden: bool,
    /// Whether the pointer is currently over the surface. Peeking wins over
    /// policy, which is what makes the trigger sliver work.
    peeking: bool,
    /// Whether something is holding the dock out: an open popover, or a drag
    /// in progress. Both take a pointer grab, which makes the dock's own
    /// motion controller report `leave` — so without this the dock slides
    /// away the instant a menu opens or a drag starts, leaving the menu
    /// floating over nothing and the drag with nowhere to drop.
    held: u32,
    /// Bumped whenever a reveal/hide timer is scheduled, so a stale timer
    /// firing after the pointer moved on is ignored.
    generation: u64,
    edge: Edge,
    base_margin: i32,
    travel: f64,
    ticking: bool,
    last_us: i64,
}

/// Distance the surface must move to be off-screen but for a trigger sliver.
fn travel_for(cfg: &Config, geom: &Geometry) -> f64 {
    let extent = if cfg.dock.position.is_vertical() { geom.window_w } else { geom.window_h };
    (extent - cfg.autohide.trigger_px.max(1) as f64).max(0.0)
}

/// Make a pinned item draggable.
///
/// Only entries that live in the pinned list can move; running-but-unpinned
/// apps, folders, Trash and the launcher have no position to rewrite.
#[allow(clippy::too_many_arguments)]
fn attach_drag(
    slot: &gtk::Overlay,
    item: &DockItem,
    state: &Rc<RefCell<State>>,
    at: &Rc<Cell<usize>>,
    cfg: &Config,
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
) {
    // Whether a slot is pinned at all is fixed for the life of its widget: a
    // reorder moves widgets around but never turns a pinned app into a folder.
    // Where it is pinned is not, so that is read live below.
    if item.pin_index.is_none() {
        return;
    }

    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);

    {
        // The payload is the pinned index, which is what the drop rewrites.
        //
        // It must be read at drag time, not captured when the slot was built.
        // A reorder permutes the widgets and leaves this controller attached,
        // so a captured index would name whatever item had since moved into
        // this slot's old position — dragging one icon would silently move
        // another.
        let state = state.clone();
        let at = at.clone();
        source.connect_prepare(move |_, _, _| {
            let pin = state.borrow().data.get(at.get())?.pin_index?;
            Some(gdk::ContentProvider::for_value(&(pin as u32).to_value()))
        });
    }

    // Drag image under the cursor.
    {
        let icon_name = item.icon.clone();
        let is_sep = item.kind == ItemKind::Separator;
        let size = cfg.dock.icon_size as i32;
        let sep_widget = slot.clone();
        source.connect_drag_begin(move |src, _| {
            if is_sep {
                // A separator has no themed icon, and leaving it unset gives
                // GTK's generic document fallback — a white page, which looks
                // like the wrong thing entirely. Paint the divider itself.
                let paintable = gtk::WidgetPaintable::new(Some(&sep_widget));
                src.set_icon(Some(&paintable), 6, size / 2);
                return;
            }
            if let Some(display) = gdk::Display::default() {
                let theme = gtk::IconTheme::for_display(&display);
                if theme.has_icon(&icon_name) {
                    let paintable = theme.lookup_icon(
                        &icon_name,
                        &[],
                        size,
                        1,
                        gtk::TextDirection::None,
                        gtk::IconLookupFlags::empty(),
                    );
                    src.set_icon(Some(&paintable), size / 2, size / 2);
                }
            }
        });
    }

    // Dim the original while it is being dragged, so the dock shows where the
    // item currently is rather than appearing to have two of it.
    {
        let state = state.clone();
        let slide = slide.clone();
        let window = window.clone();
        let cfg = cfg.clone();
        // A separator's drag image is a live paintable of the slot itself, so
        // dimming the original would dim the thing under the cursor too.
        let dim = item.kind != ItemKind::Separator;
        let at_begin = at.clone();
        source.connect_drag_begin(move |_, _| {
            if dim {
                if let Some(w) = state.borrow().items.get(at_begin.get()) {
                    w.set_opacity(0.35);
                }
            }
            // A drag grabs the pointer, so the dock would otherwise decide the
            // pointer had left and hide mid-drag.
            hold(&slide, &window, &cfg, true);
        });
    }
    {
        let state = state.clone();
        let slide = slide.clone();
        let window = window.clone();
        let cfg = cfg.clone();
        let at_end = at.clone();
        source.connect_drag_end(move |_, _, _| {
            if let Some(w) = state.borrow().items.get(at_end.get()) {
                w.set_opacity(1.0);
            }
            hold(&slide, &window, &cfg, false);
        });
    }

    slot.add_controller(source);
}

/// Which rendered slot a drop at `pos` would insert before, and the pinned
/// index that corresponds to.
///
/// Compares against slot centres so an item can be placed before the first
/// entry as well as after the last.
fn drop_position(s: &State, pos: f64, icon: f64) -> Option<(usize, usize)> {
    let horizontal = s.geom.horizontal();
    let mut result = None;
    for (i, item) in s.data.iter().enumerate() {
        let Some(pin) = item.pin_index else { continue };
        let Some((sx, sy)) = s.geom.slots.get(i).copied() else { continue };
        let extent = s.geom.extents.get(i).copied().unwrap_or(icon);
        let centre = if horizontal { sx + extent / 2.0 } else { sy + extent / 2.0 };
        if pos < centre {
            return Some((i, pin));
        }
        result = Some((i + 1, pin + 1));
    }
    result
}

/// Open or close the gap that previews where a drop will land.
fn set_drop_gap(state: &Rc<RefCell<State>>, at: Option<usize>, icon: f64) {
    {
        let mut s = state.borrow_mut();
        if s.drop_at == at {
            return;
        }
        s.drop_at = at;

        // Split the gap either side of the insertion point, so the parting is
        // symmetric and the dock does not visibly grow past its own panel.
        let gap = icon * 0.45;
        for i in 0..s.shifts.len() {
            s.shifts[i].target = match at {
                Some(at) if i >= at => gap / 2.0,
                Some(_) => -gap / 2.0,
                None => 0.0,
            };
        }
    }
    ensure_ticking(state);
}

/// Accept a dragged dock item and rewrite the pinned order.
fn attach_drop(
    fixed: &gtk::Fixed,
    state: &Rc<RefCell<State>>,
    sink: &ActionSink,
    cfg: &Config,
) {
    let target = gtk::DropTarget::new(glib::Type::U32, gdk::DragAction::MOVE);
    let state = state.clone();
    let sink = sink.clone();
    let icon = cfg.dock.icon_size;

    // Preview: part the icons at the prospective insertion point.
    {
        let state = state.clone();
        target.connect_motion(move |_, x, y| {
            let at = {
                let s = state.borrow();
                let pos = if s.geom.horizontal() { x } else { y };
                drop_position(&s, pos, icon).map(|(i, _)| i)
            };
            set_drop_gap(&state, at, icon);
            gdk::DragAction::MOVE
        });
    }
    {
        let state = state.clone();
        target.connect_leave(move |_| set_drop_gap(&state, None, icon));
    }

    {
        let state = state.clone();
        target.connect_drop(move |_, value, x, y| {
            let Ok(from) = value.get::<u32>() else { return false };
            let from = from as usize;

            let to = {
                let s = state.borrow();
                let pos = if s.geom.horizontal() { x } else { y };
                drop_position(&s, pos, icon).map(|(_, pin)| pin)
            };
            // Close the gap immediately: the rebuild that follows will place
            // everything properly, and leaving it open flashes a gap in the
            // wrong spot.
            set_drop_gap(&state, None, icon);

            let Some(to) = to else { return false };
            sink(MenuAction::ReorderPin { from, to });
            true
        });
    }

    fixed.add_controller(target);
}

/// Where an item's running indicator goes, in surface coordinates.
fn indicator_origin(geom: &Geometry, i: usize, cfg: &Config) -> Option<(f64, f64)> {
    let (len, thick) = if geom.horizontal() { (6.0, 3.0) } else { (3.0, 6.0) };
    geom.indicator_at(i, cfg.dock.icon_size, len, thick)
}

/// Which side of an icon a popover should open on, given the dock's edge.
fn popover_side(cfg: &Config) -> gtk::PositionType {
    match cfg.dock.position {
        Position::Bottom => gtk::PositionType::Top,
        Position::Top => gtk::PositionType::Bottom,
        Position::Left => gtk::PositionType::Right,
        Position::Right => gtk::PositionType::Left,
    }
}

/// Keep the dock out while a popover it spawned is open, and let it hide again
/// once that popover closes.
fn hold_for_popover(
    popover: &gtk::Popover,
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
) {
    hold(slide, window, cfg, true);

    let slide = slide.clone();
    let window = window.clone();
    let cfg = cfg.clone();
    popover.connect_closed(move |p| {
        p.unparent();
        hold(&slide, &window, &cfg, false);
    });
}

/// Whether the pointer is currently over this surface.
///
/// `device_position` yields None when the pointer is over some other surface,
/// which is exactly the "the dock may hide" case.
fn pointer_inside(window: &gtk::ApplicationWindow) -> bool {
    let Some(surface) = window.surface() else { return false };
    let Some(seat) = gdk::Display::default().and_then(|d| d.default_seat()) else {
        return false;
    };
    let Some(pointer) = seat.pointer() else { return false };
    surface.device_position(&pointer).is_some()
}

/// Take or release a hold on the dock, keeping it out while one is active.
///
/// Counted rather than boolean: a drag can begin from a slot while a popover
/// is still closing, and a plain flag would let the first release drop the
/// dock out from under the second holder.
fn hold(
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
    take: bool,
) {
    // A drag or popover grabs the pointer, which makes the dock's motion
    // controller report a `leave` that never really happened. By the time the
    // hold is released that stale `peeking = false` would hide the dock out
    // from under a pointer still sitting on it — so re-derive it from where
    // the pointer actually is.
    let inside = if take { None } else { Some(pointer_inside(window)) };

    {
        let mut s = slide.borrow_mut();
        if take {
            s.held += 1;
        } else {
            s.held = s.held.saturating_sub(1);
            if let Some(inside) = inside {
                if s.held == 0 {
                    s.peeking = inside;
                }
            }
        }
        let target = if s.hidden && !s.peeking && s.held == 0 { s.travel } else { 0.0 };
        if (s.spring.target - target).abs() < f64::EPSILON {
            return;
        }
        s.spring.target = target;
    }
    animate_slide_on(slide, window, cfg);
}

/// Give an item an upward impulse and make sure the tick loop is running.
fn kick(state: &Rc<RefCell<State>>, index: usize) {
    {
        let mut s = state.borrow_mut();
        if index >= s.bounces.len() {
            return;
        }
        s.bounces[index].vel = BOUNCE_IMPULSE;
        s.bounces[index].target = 0.0;
    }
    ensure_ticking(state);
}

/// Anchor the surface to the configured screen edge.
fn init_layer_shell(
    window: &gtk::ApplicationWindow,
    cfg: &Config,
    monitor: Option<&gdk::Monitor>,
) -> Edge {
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
    window.set_margin(edge, 0);

    if cfg.dock.reserve_space {
        window.auto_exclusive_zone_enable();
    } else {
        // Float over windows without reserving screen space.
        window.set_exclusive_zone(0);
    }
    edge
}

/// Build an icon image from a desktop entry's `Icon=` value.
///
/// That value may be a themed icon name or an absolute path, and the state
/// engine has already resolved it, so this only has to handle both forms and
/// fall back when the theme lacks the name.
/// The fill drawn behind a hovered slot, placed and hidden.
///
/// Created for every slot rather than on demand: the tick callback animates it
/// by opacity, and a widget that has to be built mid-hover would cost a
/// layout pass on the first frame of every hover.
fn hover_plate(fixed: &gtk::Fixed, x: f64, y: f64, w: f64, h: f64) -> gtk::Widget {
    let plate = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    plate.add_css_class("dock-hover-plate");
    plate.set_size_request(w.round() as i32, h.round() as i32);
    // Fully transparent rather than hidden: opacity is what the spring drives,
    // and a hidden widget would pop into place on the first frame.
    plate.set_opacity(0.0);
    plate.set_can_target(false);
    fixed.put(&plate, x, y);
    plate.upcast::<gtk::Widget>()
}

/// What a slot draws: a themed application icon, or a monochrome glyph.
///
/// The dock's own furniture — launcher, folder stacks, Trash — is drawn the
/// way the bar draws its widgets, so only real applications carry colour. An
/// item without a glyph, or a config that turned glyphs off, falls back to the
/// icon theme.
fn item_visual(item: &DockItem, size: i32, cfg: &Config) -> gtk::Widget {
    match item.glyph.as_deref().filter(|_| cfg.items.glyph_ui) {
        Some(glyph) => {
            let label = gtk::Label::new(Some(glyph));
            label.add_css_class("dock-glyph");
            // Glyphs are drawn by the font, so the size has to come from the
            // type scale rather than from a pixel-size request. The ratio
            // leaves the same optical weight as a themed icon of `size`.
            label.set_attributes(Some(&glyph_attrs(size)));
            label.set_size_request(size, size);
            label.upcast::<gtk::Widget>()
        }
        None => make_icon(&item.icon, size).upcast::<gtk::Widget>(),
    }
}

/// Pango attributes sizing a glyph to fill an icon box.
fn glyph_attrs(size: i32) -> gtk::pango::AttrList {
    let attrs = gtk::pango::AttrList::new();
    // 0.62 of the box: a Nerd Font glyph's ink sits well inside its em, so
    // matching the point size to the box would draw it noticeably small.
    let points = (size as f64 * 0.62).round() as i32;
    attrs.insert(gtk::pango::AttrSize::new(points * gtk::pango::SCALE));
    attrs
}

fn make_icon(icon: &str, size: i32) -> gtk::Image {
    let img = gtk::Image::new();
    img.set_pixel_size(size);
    img.add_css_class("dock-icon");

    if icon.starts_with('/') {
        img.set_from_file(Some(icon));
        return img;
    }

    let has = gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d))
        .is_some_and(|t| t.has_icon(icon));

    img.set_icon_name(Some(if has { icon } else { "application-x-executable" }));
    img
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
    let (delay, generation) = {
        let mut s = state.borrow_mut();
        if s.hovered == hit {
            return;
        }
        s.hovered = hit;
        s.retarget();
        // Any pending tooltip belongs to the slot we just left.
        s.tip_generation += 1;
        s.tip_label.set_visible(false);
        (s.tooltip_delay, s.tip_generation)
    };
    ensure_ticking(state);

    let Some(index) = hit else { return };

    let st = state.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        let s = st.borrow();
        // Discard a timer whose hover has since moved on.
        if s.tip_generation != generation || s.hovered != Some(index) {
            return;
        }
        let Some(item) = s.data.get(index) else { return };
        if !item.interactive() || item.label.is_empty() {
            return;
        }

        let text = if item.windows.len() > 1 {
            format!("{} ({} windows)", item.label, item.windows.len())
        } else {
            item.label.clone()
        };
        s.tip_label.set_text(&text);
        s.tip_label.set_visible(true);

        // Centre the label over its icon, then keep it inside the surface so a
        // long name on the first or last icon is not cut off.
        let (_, width, _, _) = s.tip_label.measure(gtk::Orientation::Horizontal, -1);
        let width = width as f64;
        let Some((sx, _)) = s.geom.slots.get(index).copied() else { return };
        let extent = s.geom.extents.get(index).copied().unwrap_or(0.0);
        let x = (sx + extent / 2.0 - width / 2.0).clamp(0.0, (s.geom.window_w - width).max(0.0));
        s.fixed.move_(&s.tip_label, x, 2.0);
    });
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
            let zoom_busy = !s.springs[i].settled();
            let bounce_busy = !s.bounces[i].settled();
            let shift_busy = !s.shifts[i].settled();
            let hover_busy = !s.hovers[i].settled();
            if !zoom_busy && !bounce_busy && !shift_busy && !hover_busy {
                continue;
            }

            if hover_busy {
                s.hovers[i].step(dt, HOVER_STIFFNESS, HOVER_DAMPING);
                if s.hovers[i].settled() {
                    s.hovers[i].settle();
                } else {
                    moving = true;
                }
            }

            if zoom_busy {
                s.springs[i].step_cfg(dt, &cfg);
                if s.springs[i].settled() {
                    s.springs[i].settle();
                } else {
                    moving = true;
                }
            }
            if bounce_busy {
                // Softer and less damped than the zoom spring, so a launch
                // reads as a bounce rather than a nudge.
                s.bounces[i].step(dt, BOUNCE_STIFFNESS, BOUNCE_DAMPING);
                if s.bounces[i].settled() {
                    s.bounces[i].settle();
                } else {
                    moving = true;
                }
            }
            if shift_busy {
                // Critically damped: the gap should part cleanly and hold,
                // not wobble while the user is aiming a drop.
                s.shifts[i].step(dt, SHIFT_STIFFNESS, SHIFT_DAMPING);
                if s.shifts[i].settled() {
                    s.shifts[i].settle();
                } else {
                    moving = true;
                }
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
