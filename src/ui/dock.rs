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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::anim::Spring;
use crate::config::{Config, Hover, Position};
use crate::runtime::DockCommand;
use crate::state::{DockItem, ItemKind};
use crate::ui::menu::{self, MenuAction};
use crate::ui::preview::{self, Panel};
use crate::ui::Geometry;

/// Actions a dock surface emits. The app owns the worker channels and config,
/// so the UI reports intent rather than acting on it.
pub type ActionSink = Rc<dyn Fn(MenuAction)>;

/// Must match the Hyprland `layerrule` namespace.
pub const LAYER_NAMESPACE: &str = "omarchy-dock";

/// Launch feedback: the slot's hover fill breathes while the app starts.
///
/// Not a bounce. Omarchy's shell never overshoots — everything it animates is
/// a short ease — so a springy macOS bounce was the one motion in the dock
/// that did not belong. A fill that breathes on a sine ease-in-out is the
/// shell's own vocabulary, and it still says "your click worked" during the
/// seconds an Electron app takes to show a window.
///
/// One breath, peak to trough and back.
const PULSE_PERIOD_S: f64 = 1.1;
/// The fill's floor mid-breath: dim, but never gone, so it reads as one
/// continuous state rather than blinking.
const PULSE_FLOOR: f64 = 0.3;
/// An app that never shows a window stops breathing after this long.
const PULSE_TIMEOUT_S: f64 = 10.0;

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

/// How long window previews stay up after the pointer leaves their icon. Long
/// enough to cross the gap between the dock and the strip, short enough that
/// sweeping past an icon does not leave a strip behind.
const PREVIEW_LINGER_MS: u64 = 250;

struct State {
    fixed: gtk::Fixed,
    /// Current item data. Click handlers read this by index rather than
    /// capturing a copy, so an in-place refresh cannot leave them stale.
    data: Vec<DockItem>,
    /// Per-slot indicator and badge widgets. Always created, shown or hidden
    /// as state changes, so a refresh never has to build widgets.
    indicators: Vec<gtk::Widget>,
    badges: Vec<gtk::Label>,
    /// Per-slot media progress ring and the (progress, playing) it draws.
    /// Created for every app slot and shown only while a player belongs to
    /// it, so a track starting never needs new widgets.
    rings: Vec<Option<MediaRing>>,
    /// Per-slot download spinner, for folder stacks: turns while the
    /// Downloads folder has downloads in progress.
    spinners: Vec<Option<Spinner>>,
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
    /// Launch feedback per slot, while it lasts, and the fill level it is
    /// currently drawing.
    pulses: Vec<Option<Pulse>>,
    pulse_levels: Vec<f64>,
    /// Sideways displacement along the dock's long axis, used to open a gap at
    /// the drop position while dragging. Its own spring so it composes with
    /// magnification rather than fighting it.
    shifts: Vec<Spring>,
    /// Rendered index the drop would insert before, while a drag is over the
    /// dock.
    drop_at: Option<usize>,
    geom: Geometry,
    cfg: Config,
    hovered: Option<usize>,
    last_us: i64,
    ticking: bool,
    /// Window previews, when enabled. Outside the `RefCell`'s reach in
    /// practice: callers clone the `Rc` out and drop their borrow before using
    /// it, because showing and hiding the strip calls back into the dock.
    previews: Option<Rc<Previews>>,
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

        let shift = self.shifts[i].pos;
        let zoom = self.cfg.magnify.zoom;
        // 0..1 as the spring travels from rest to full zoom.
        let p = if zoom > 1.0 { ((s.pos - 1.0) / (zoom - 1.0)).clamp(0.0, 1.0) } else { 0.0 };
        let lift = self.cfg.magnify.lift * p;
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
        // a drop gap, but it deliberately does not zoom or lift: it is
        // the seat the icon sits in, not part of the icon.
        if let Some(plate) = self.plates.get(i) {
            let (px, py) = plate_origin(&self.geom, i);
            let shift = self.shifts[i].pos;
            let (dx, dy) = if self.geom.horizontal() { (shift, 0.0) } else { (0.0, shift) };
            self.fixed.set_child_transform(
                plate,
                Some(&gsk::Transform::new().translate(&graphene::Point::new(
                    (px + dx) as f32,
                    (py + dy) as f32,
                ))),
            );
            // A launch pulse takes over the fill while it runs, hovered or
            // not: the pointer usually still rests on the icon it clicked.
            let level =
                if self.pulses[i].is_some() { self.pulse_levels[i] } else { self.hovers[i].pos };
            plate.set_opacity(level.clamp(0.0, 1.0));
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
        // Slot order, for the test hooks that address items by index.
        tracing::debug!(
            slots = ?items.iter().enumerate().map(|(i, it)| format!("{i}:{}", it.key)).collect::<Vec<_>>(),
            "building dock surface"
        );
        let travel = travel_for(cfg, &geom);
        let extent = extent_for(cfg, &geom);
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
        let mut rings: Vec<Option<MediaRing>> = Vec::with_capacity(items.len());
        let mut spinners: Vec<Option<Spinner>> = Vec::with_capacity(items.len());

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
                let (px, py) = plate_origin(&geom, i);
                plates.push(hover_plate(&fixed, px, py, 0.0, 0.0));
                rings.push(None);
                spinners.push(None);
                let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                dot.set_visible(false);
                indicators.push(dot.upcast::<gtk::Widget>());
                let badge = gtk::Label::new(None);
                badge.set_visible(false);
                badges.push(badge);
                continue;
            }

            // A workspace tile is narrower than an icon, so the slot takes its
            // own extent rather than assuming a square.
            let extent = geom.extents.get(i).copied().unwrap_or(cfg.dock.icon_size);
            let (slot_w, slot_h) = if geom.horizontal() {
                (extent.round() as i32, size)
            } else {
                (size, extent.round() as i32)
            };
            slot.set_size_request(slot_w, slot_h);

            // The plate goes in first so it draws beneath this slot. Sized a
            // little tighter than the icon box, the way a bar widget's
            // highlight sits inside its cell rather than filling it.
            let (px, py) = plate_origin(&geom, i);
            plates.push(hover_plate(
                &fixed,
                px,
                py,
                slot_w as f64 + PLATE_MARGIN * 2.0,
                slot_h as f64 + PLATE_MARGIN * 2.0,
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
            set_class(&badge, "unread", item.unread > 0);
            slot.add_overlay(&badge);
            badges.push(badge);

            let ring = (item.kind == ItemKind::App).then(|| media_ring(size));
            if let Some(r) = &ring {
                slot.add_overlay(&r.area);
                r.set(item.media.as_ref());
            }
            rings.push(ring);

            let spin = (item.kind == ItemKind::Folder).then(|| spinner(size));
            if let Some(sp) = &spin {
                slot.add_overlay(&sp.area);
                sp.set(item.downloading > 0);
            }
            spinners.push(spin);

            let (x, y) = geom.slots[i];
            fixed.put(&slot, x, y);
            slots.push(slot.clone());
            widgets.push(slot.upcast::<gtk::Widget>());

            // The indicator is a separate, untransformed child: on macOS the
            // running dot stays put while the icon above it grows.
            let (len, thick) = if geom.horizontal() { (6.0, 3.0) } else { (3.0, 6.0) };
            let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            dot.add_css_class("dock-indicator");
            // Set here rather than left to the first refresh: a dot created
            // visible shows under every item until something else triggers an
            // update, which is how stray dots appeared on every icon once.
            dot.set_visible(item.shows_indicator());
            set_class(&dot, "urgent", item.urgent);
            set_class(&dot, "active", item.active);
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
            pulses: vec![None; widget_count],
            pulse_levels: vec![0.0; widget_count],
            shifts: vec![Spring::at(0.0); widget_count],
            drop_at: None,
            data: items.to_vec(),
            indicators,
            badges,
            rings,
            spinners,
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
            previews: None,
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
            settle_gen: 0,
            edge,
            // Zero: the edge offset lives inside the surface now, so the
            // surface itself is flush with the screen edge.
            base_margin: 0,
            // How far the surface must travel to be off-screen, minus the
            // sliver left behind as a pointer trigger.
            travel,
            extent,
            away: false,
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

        // Clicks are wired after State exists so a launch can pulse its own
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
        attach_file_drop(&fixed, &state, &sink, &slide, &window, cfg);
        attach_workspace_scroll(&fixed, &state, &sink);
        if cfg.preview.enabled {
            let previews = Previews::new(app, monitor, &state, &sink, &slide, &window, cfg);
            state.borrow_mut().previews = Some(previews);
        }

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
                let st_menu = state.clone();
                let (slide_m, window_m, cfg_m) = (slide.clone(), window.clone(), cfg.clone());
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(600),
                    move || {
                        let Some(anchor) = anchor else { return };
                        let item = st_menu.borrow().data.get(i).cloned();
                        let Some(pop) = item.and_then(|it| context_menu(&it, &sink)) else { return };
                        pop.set_parent(&anchor);
                        pop.set_position(side);
                        // Hold the dock out, exactly as a real right-click does.
                        hold_for_popover(&pop, &slide_m, &window_m, &cfg_m);
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

        // Test hook: a click cannot be synthesised either, so this starts the
        // launch pulse on the given slot, as launching it would.
        if let Ok(n) = std::env::var("OMARCHY_DOCK_FORCE_PULSE") {
            if let Ok(i) = n.parse::<usize>() {
                let st = state.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(400),
                    move || pulse(&st, i),
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

        surface
    }

    pub fn close(&self) {
        let previews = self.state.borrow().previews.clone();
        if let Some(p) = previews {
            p.close();
        }
        self.window.close();
    }

    /// Refresh indicators, badges and tooltips without rebuilding.
    ///
    /// Returns false when the item *set* changed (different apps, or a
    /// different order), which needs new widgets. Rebuilding on every focus
    /// change would destroy and recreate the layer surface — losing slide
    /// state and flickering — so only shape changes pay that cost.
    pub fn refresh(&self, items: &[DockItem]) -> bool {
        let fresh = self.refresh_in_place(items);
        if fresh {
            // A window opened, closed or retitled under an open strip would
            // leave it showing what was, so it is redrawn from the new data.
            let previews = self.state.borrow().previews.clone();
            if let Some(p) = previews {
                p.refresh(&self.state);
            }
        }
        fresh
    }

    fn refresh_in_place(&self, items: &[DockItem]) -> bool {
        let mut s = self.state.borrow_mut();
        if s.data.len() != items.len()
            || !s.data.iter().zip(items).all(|(a, b)| a.key == b.key)
        {
            return false;
        }

        for (i, item) in items.iter().enumerate() {
            if let Some(slot) = s.items.get(i) {
                sync_workspace_tile(slot, item);
            }
            if let Some(Some(ring)) = s.rings.get(i) {
                ring.set(item.media.as_ref());
            }
            if let Some(Some(sp)) = s.spinners.get(i) {
                sp.set(item.downloading > 0);
            }
            if let Some(dot) = s.indicators.get(i) {
                dot.set_visible(item.shows_indicator());
                // Toggle rather than add: classes persist across refreshes.
                set_class(dot, "urgent", item.urgent);
                set_class(dot, "active", item.active);
            }
            if let Some(badge) = s.badges.get(i) {
                set_class(badge, "unread", item.unread > 0);
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
    pub fn reorder(&self, items: &[DockItem]) -> bool {
        // The geometry this surface was built with — fitted to its monitor —
        // not the configured one, or a reorder would undo the fit.
        let cfg = self.state.borrow().cfg.clone();
        let cfg = &cfg;
        // The strip names a slot by index, which is about to mean another item.
        let previews = self.state.borrow().previews.clone();
        if let Some(p) = previews {
            p.hide();
        }
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
        s.rings = from.iter().map(|&i| s.rings[i].clone()).collect();
        s.spinners = from.iter().map(|&i| s.spinners[i].clone()).collect();
        s.springs = from.iter().map(|&i| s.springs[i]).collect();
        s.pulses = from.iter().map(|&i| s.pulses[i]).collect();
        s.pulse_levels = from.iter().map(|&i| s.pulse_levels[i]).collect();
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
            if let Some(slot) = s.items.get(i) {
                sync_workspace_tile(slot, item);
            }
            if let Some((ix, iy)) = indicator_origin(&s.geom, i, cfg) {
                if let Some(dot) = s.indicators.get(i) {
                    s.fixed.move_(dot, ix, iy);
                    dot.set_visible(item.shows_indicator());
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

    /// Give the item with `key` one breath of the launch pulse.
    pub fn pulse_key(&self, key: &str) {
        let at = self.state.borrow().data.iter().position(|d| d.key == key);
        if let Some(i) = at {
            pulse(&self.state, i);
        }
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

    /// Take the surface off-screen whole, or let auto-hide have it back.
    ///
    /// For the screensaver: a hidden dock keeps a sliver that reveals it on
    /// hover, and the pointer resting at the bottom edge of a screensaver
    /// should not summon anything.
    pub fn set_away(&self, away: bool, cfg: &Config) {
        {
            let mut s = self.slide.borrow_mut();
            if s.away == away {
                return;
            }
            s.away = away;
            // Whatever the pointer was doing before is stale by the time the
            // dock comes back; a fresh enter on the sliver peeks again.
            s.peeking = false;
        }
        self.apply_slide(cfg);
    }

    /// Drive the spring toward wherever policy and peek state agree it goes.
    fn apply_slide(&self, cfg: &Config) {
        {
            let mut s = self.slide.borrow_mut();
            let target = s.target();
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
            hide_previews(&state);

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
                ItemKind::Workspace => {
                    if let Some(name) = crate::state::workspace_of(&item.key) {
                        sink(MenuAction::Command(DockCommand::FocusWorkspace(
                            name.to_string(),
                        )));
                    }
                    return;
                }
                ItemKind::Scratchpad => {
                    sink(MenuAction::Command(DockCommand::ToggleSpecial(
                        crate::state::SCRATCHPAD.to_string(),
                    )));
                    return;
                }
                ItemKind::Tray => {
                    // Handed straight to the application: a tray icon's click
                    // means whatever that application decided it means.
                    if let Some(service) = crate::state::tray_service(&item.key) {
                        sink(MenuAction::Command(DockCommand::TrayClick {
                            service: service.to_string(),
                            click: crate::tray::Click::Primary,
                        }));
                    }
                    return;
                }
                ItemKind::Recording => {
                    sink(MenuAction::Command(DockCommand::Exec(item.exec.clone())));
                    return;
                }
                ItemKind::Command => {
                    if !item.exec.is_empty() {
                        pulse(&state, index);
                        sink(MenuAction::Command(DockCommand::Exec(item.exec.clone())));
                    }
                    return;
                }
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
                    pulse(&state, index);
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

    // ── middle button ───────────────────────────────────────────────────────
    // A new window of an app, as in most docks; for a tray item, the
    // secondary action its application defines.
    {
        let middle = gtk::GestureClick::new();
        middle.set_button(gdk::BUTTON_MIDDLE);
        let sink = sink.clone();
        let state = state.clone();
        let at = at.clone();
        middle.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let index = at.get();
            let Some(item) = state.borrow().data.get(index).cloned() else { return };
            if let Some(cmd) = item.new_window_command() {
                hide_previews(&state);
                pulse(&state, index);
                sink(MenuAction::Command(DockCommand::Exec(cmd)));
                return;
            }
            if item.kind != ItemKind::Tray {
                return;
            }
            if let Some(service) = crate::state::tray_service(&item.key) {
                sink(MenuAction::Command(DockCommand::TrayClick {
                    service: service.to_string(),
                    click: crate::tray::Click::Middle,
                }));
            }
        });
        slot.add_controller(middle);
    }

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
            hide_previews(&state);
            let item = {
                let s = state.borrow();
                match s.data.get(at.get()) {
                    Some(i) => i.clone(),
                    None => return,
                }
            };
            let sink = sink.clone();
            // A tray item's menu belongs to its application: it is served over
            // DBusMenu, and redrawing it here would mean reproducing another
            // program's UI and getting it subtly wrong. Ask the app to post it.
            if item.kind == ItemKind::Tray {
                if let Some(service) = crate::state::tray_service(&item.key) {
                    sink(MenuAction::Command(DockCommand::TrayClick {
                        service: service.to_string(),
                        click: crate::tray::Click::Secondary,
                    }));
                }
                return;
            }

            let Some(popover) = context_menu(&item, &sink) else { return };
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

/// The popover a right-click on `item` opens, if it has one.
///
/// Separate from the click handler so the test hook opens exactly the menu a
/// real right-click would.
fn context_menu(item: &DockItem, sink: &ActionSink) -> Option<gtk::Popover> {
    // The stop button does one thing; a menu would only repeat it.
    if item.kind == ItemKind::Recording {
        return None;
    }
    let sink = sink.clone();
            let popover = if item.kind == ItemKind::Separator {
                // Only user-placed separators are editable; automatic dividers
                // are derived from the item list and have no pinned index.
                menu::build_separator(item.pin_index?, move |a| sink(a))
            } else if item.kind == ItemKind::Trash || item.kind == ItemKind::Folder {
                let sink2 = sink.clone();
                let refresh = move || sink2(MenuAction::Rescan);
                if item.kind == ItemKind::Trash {
                    crate::ui::stack::build_trash(refresh)
                } else {
                    crate::ui::stack::build_folder(item.path.as_ref()?, &item.label, refresh)
                }
            } else if item.kind == ItemKind::Launcher {
                // The launcher has no windows or desktop actions, so its
                // right-click is the natural home for the dock's own settings.
                crate::ui::settings::build(move |a| sink(a))
            } else {
                menu::build(item, move |a| sink(a))
            };
            Some(popover)
}

/// Re-apply a workspace tile's occupancy and current-workspace styling.
///
/// The tile's state lives on the label inside the slot, which a refresh would
/// otherwise leave untouched: the key sequence does not change when you switch
/// workspace, so no rebuild happens and the strip would keep showing whichever
/// workspace was current when the dock was built.
fn sync_workspace_tile(slot: &gtk::Widget, item: &DockItem) {
    if item.kind != ItemKind::Workspace {
        return;
    }
    let Some(label) = slot
        .downcast_ref::<gtk::Overlay>()
        .and_then(|o| o.child())
        .and_downcast::<gtk::Label>()
    else {
        return;
    };
    set_class(&label, "occupied", !item.windows.is_empty());
    set_class(&label, "current", item.active);
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
        let target = s.target();
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
    /// Bumped whenever a hold is released, so only the latest release's
    /// deferred pointer check acts. Separate from `generation`, which a
    /// pending reveal timer depends on.
    settle_gen: u64,
    /// Out of the way entirely, over the screensaver: no trigger sliver, and
    /// neither hover nor an open menu brings the dock back.
    away: bool,
    edge: Edge,
    base_margin: i32,
    travel: f64,
    /// The surface's full depth: how far it moves to leave the screen whole.
    extent: f64,
    ticking: bool,
    last_us: i64,
}

impl Slide {
    /// Where the slide should settle, from policy, peek and hold state.
    fn target(&self) -> f64 {
        if self.away {
            self.extent
        } else if self.hidden && !self.peeking && self.held == 0 {
            self.travel
        } else {
            0.0
        }
    }
}

/// The surface's depth away from its screen edge.
fn extent_for(cfg: &Config, geom: &Geometry) -> f64 {
    if cfg.dock.position.is_vertical() { geom.window_w } else { geom.window_h }
}

/// Distance the surface must move to be off-screen but for a trigger sliver.
fn travel_for(cfg: &Config, geom: &Geometry) -> f64 {
    (extent_for(cfg, geom) - cfg.autohide.trigger_px.max(1) as f64).max(0.0)
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
            if let Some(file) = crate::ui::app_icon_paintable(&icon_name, size) {
                src.set_icon(Some(&file), size / 2, size / 2);
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

/// The slot of the workspace (or scratchpad) tile under a surface point.
fn workspace_slot(s: &State, x: f64, y: f64) -> Option<usize> {
    let i = s.geom.slot_at(x, y, s.cfg.dock.icon_size)?;
    matches!(s.data.get(i)?.kind, ItemKind::Workspace | ItemKind::Scratchpad).then_some(i)
}

/// The workspace tile under a surface point, if the point is on one.
fn workspace_under(s: &State, x: f64, y: f64) -> Option<String> {
    let i = workspace_slot(s, x, y)?;
    let item = s.data.get(i)?;
    match item.kind {
        ItemKind::Workspace => {
            crate::state::workspace_of(&item.key).map(|n| n.to_string())
        }
        // The scratchpad is a workspace too, and stashing a window in it by
        // dropping is the obvious gesture.
        ItemKind::Scratchpad => Some(format!("special:{}", crate::state::SCRATCHPAD)),
        _ => None,
    }
}

/// The command for dropping the item dragged from pinned index `from` onto
/// whatever workspace tile sits at `(x, y)`.
///
/// `None` when the drop is not over a workspace, or when the dragged item has
/// no window to send — dropping a pinned-but-closed app somewhere cannot mean
/// anything, and silently doing nothing is better than launching it.
fn send_to_workspace(s: &State, from: usize, x: f64, y: f64) -> Option<DockCommand> {
    let workspace = workspace_under(s, x, y)?;
    let item = s.data.iter().find(|d| d.pin_index == Some(from))?;
    // The focused window of that app if it has one, else its first: the same
    // choice a click makes.
    let window = item.active_window.clone().or_else(|| item.windows.first().cloned())?;
    Some(DockCommand::SendToWorkspace { window, workspace })
}

/// Open or close the gap that previews where a drop will land./// Open or close the gap that previews where a drop will land.
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
            let (at, tile) = {
                let s = state.borrow();
                // Over a workspace tile the drop is a "send there", so the
                // icons must not part as if something were being inserted.
                match workspace_slot(&s, x, y) {
                    Some(tile) => (None, Some(tile)),
                    None => {
                        let pos = if s.geom.horizontal() { x } else { y };
                        (drop_position(&s, pos, icon).map(|(i, _)| i), None)
                    }
                }
            };
            set_drop_gap(&state, at, icon);
            // A drag carries no pointer motion, so the tile would not light up
            // the way it does under the mouse; the target says which one the
            // window would go to. No previews mid-drag.
            hover_to(&state, tile, false);
            gdk::DragAction::MOVE
        });
    }
    {
        let state = state.clone();
        target.connect_leave(move |_| {
            set_drop_gap(&state, None, icon);
            hover_to(&state, None, false);
        });
    }

    {
        let state = state.clone();
        target.connect_drop(move |_, value, x, y| {
            let Ok(from) = value.get::<u32>() else { return false };
            let from = from as usize;

            // Dropping an app onto a workspace tile means "put this there",
            // not "reorder the pins". Checked first because a workspace tile
            // occupies a slot the reorder logic would otherwise read as an
            // insertion point.
            // Bound first: a borrow in an `if let` condition lives for the
            // whole block, and set_drop_gap below needs to borrow mutably —
            // which aborted the dock on every drop onto a workspace.
            let send = send_to_workspace(&state.borrow(), from, x, y);
            hover_to(&state, None, false);
            if let Some(cmd) = send {
                set_drop_gap(&state, None, icon);
                sink(MenuAction::Command(cmd));
                return true;
            }

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

/// What a file dragged over slot `i` would do there, if anything.
enum FileDrop {
    /// Open the files with this app's `Exec=` line.
    Open(String),
    /// Move them to the Trash.
    Trash,
}

fn file_drop_at(s: &State, x: f64, y: f64) -> Option<(usize, FileDrop)> {
    let i = s.geom.slot_at(x, y, s.cfg.dock.icon_size)?;
    let item = s.data.get(i)?;
    match item.kind {
        ItemKind::Trash => Some((i, FileDrop::Trash)),
        _ => item.open_with.clone().map(|exec| (i, FileDrop::Open(exec))),
    }
}

/// Accept files dragged in from a file manager or anywhere else.
///
/// Dropped on an app that declares it opens files, they open with it; dropped
/// on Trash, they are trashed. Everything else refuses the drop outright, so
/// the cursor says "no" before release rather than the drop silently doing
/// nothing. A hidden dock reveals itself while a file is dragged over its edge
/// and stays out for the length of the drag.
fn attach_file_drop(
    fixed: &gtk::Fixed,
    state: &Rc<RefCell<State>>,
    sink: &ActionSink,
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
) {
    let target = gtk::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    // Whether this drag currently holds the dock out, so enter/leave/drop can
    // never release a hold they did not take, or take two.
    let holding = Rc::new(Cell::new(false));

    let release = {
        let (holding, slide, window, cfg) = (holding.clone(), slide.clone(), window.clone(), cfg.clone());
        move || {
            if holding.replace(false) {
                hold(&slide, &window, &cfg, false);
            }
        }
    };

    {
        let (holding, slide, window, cfg) = (holding.clone(), slide.clone(), window.clone(), cfg.clone());
        target.connect_enter(move |_, _, _| {
            if !holding.replace(true) {
                hold(&slide, &window, &cfg, true);
            }
            // Report nothing yet: the motion handler decides per slot.
            gdk::DragAction::empty()
        });
    }
    {
        let state = state.clone();
        target.connect_motion(move |_, x, y| {
            let hit = file_drop_at(&state.borrow(), x, y);
            // The hover plate and the name label are exactly the feedback a
            // drop target needs: this icon, and what it is called.
            // No previews, though: a strip popping up over the drop target
            // mid-drag would cover the very thing being aimed at.
            hover_to(&state, hit.as_ref().map(|(i, _)| *i), false);
            match hit {
                Some((_, FileDrop::Open(_))) => gdk::DragAction::COPY,
                Some((_, FileDrop::Trash)) => gdk::DragAction::MOVE,
                None => gdk::DragAction::empty(),
            }
        });
    }
    {
        let state = state.clone();
        let release = release.clone();
        target.connect_leave(move |_| {
            set_hover(&state, None);
            release();
        });
    }
    {
        let state = state.clone();
        let sink = sink.clone();
        target.connect_drop(move |_, value, x, y| {
            let hit = file_drop_at(&state.borrow(), x, y);
            set_hover(&state, None);
            release();
            let (Some((i, what)), Ok(list)) = (hit, value.get::<gdk::FileList>()) else {
                return false;
            };
            let files = list.files();
            match what {
                FileDrop::Open(exec) => {
                    let pairs: Vec<(String, String)> = files
                        .iter()
                        // A path that is not UTF-8 cannot be passed on intact, and
                        // a lossy copy would name some other file; such files
                        // are skipped.
                        .filter_map(|f| Some((f.path()?.to_str()?.to_owned(), f.uri().to_string())))
                        .collect();
                    let cmds = crate::desktop::open_command(&exec, &pairs);
                    if cmds.is_empty() {
                        return false;
                    }
                    pulse(&state, i);
                    for cmd in cmds {
                        sink(MenuAction::Command(DockCommand::Exec(cmd)));
                    }
                }
                FileDrop::Trash => {
                    let moved = files
                        .iter()
                        .filter(|f| f.trash(None::<&gio::Cancellable>).is_ok())
                        .count();
                    if moved == 0 {
                        return false;
                    }
                    crate::omarchy::notify(
                        "Moved to Trash",
                        Some(&if moved == 1 { "1 item".to_string() } else { format!("{moved} items") }),
                        Some("\u{f1f8}"),
                    );
                    // The Trash icon switches between empty and full.
                    sink(MenuAction::Rescan);
                }
            }
            true
        });
    }

    fixed.add_controller(target);
}

/// How far a touchpad must scroll, in pixels, to move one workspace. A wheel
/// moves one per notch; a touchpad reports a stream of small deltas, and
/// taking each as a step would fling through every workspace in one swipe.
const SCROLL_STEP_PX: f64 = 40.0;

/// The workspace `step` tiles away from the current one, wrapping around, as
/// Omarchy's own SUPER + scroll does. `names` are the strip's workspaces in
/// order, `current` the index of the one in front (if it is on the strip).
fn scrolled_workspace(names: &[String], current: Option<usize>, step: i32) -> Option<String> {
    if names.is_empty() || step == 0 {
        return None;
    }
    let n = names.len() as i32;
    let from = match current {
        Some(i) => i as i32,
        // Somewhere off the strip: stepping forward starts at the first tile,
        // stepping back at the last.
        None if step > 0 => -1,
        None => n,
    };
    Some(names[(from + step).rem_euclid(n) as usize].clone())
}

/// Scrolling over the workspace strip switches workspace: down for the next,
/// up for the previous, the direction Omarchy's SUPER + scroll uses.
fn attach_workspace_scroll(fixed: &gtk::Fixed, state: &Rc<RefCell<State>>, sink: &ActionSink) {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let pending = Rc::new(Cell::new(0.0_f64));
    let state = state.clone();
    let sink = sink.clone();
    scroll.connect_scroll(move |c, _, dy| {
        let (names, current) = {
            let s = state.borrow();
            // Only over the strip itself; elsewhere a scroll means nothing.
            let on_strip = s.hovered.and_then(|i| s.data.get(i)).is_some_and(|d| {
                matches!(d.kind, ItemKind::Workspace | ItemKind::Scratchpad)
            });
            if !on_strip {
                pending.set(0.0);
                return glib::Propagation::Proceed;
            }
            let tiles: Vec<&DockItem> =
                s.data.iter().filter(|d| d.kind == ItemKind::Workspace).collect();
            let names: Vec<String> = tiles
                .iter()
                .filter_map(|d| crate::state::workspace_of(&d.key).map(str::to_string))
                .collect();
            (names, tiles.iter().position(|d| d.active))
        };

        let per_step = if c.unit() == gdk::ScrollUnit::Wheel { 1.0 } else { SCROLL_STEP_PX };
        let total = pending.get() + dy;
        let steps = (total / per_step).trunc();
        pending.set(total - steps * per_step);
        if let Some(name) = scrolled_workspace(&names, current, steps as i32) {
            sink(MenuAction::Command(DockCommand::FocusWorkspace(name)));
        }
        glib::Propagation::Stop
    });
    fixed.add_controller(scroll);
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
/// How long after a grab ends before "the pointer is not over the dock" is
/// believed.
///
/// During a Wayland drag the compositor sends drag events rather than pointer
/// events, so GTK considers the pointer gone. When the drag ends, the pointer
/// enter that follows arrives a little later — asking at that instant says
/// "outside" for a pointer sitting on the dock, which hid the dock on every
/// drop wherever a window made auto-hide want it hidden, and brought it back a
/// moment later when the enter arrived.
const GRAB_SETTLE_MS: u64 = 250;

fn hold(
    slide: &Rc<RefCell<Slide>>,
    window: &gtk::ApplicationWindow,
    cfg: &Config,
    take: bool,
) {
    // A drag or popover grabs the pointer, which makes the dock's motion
    // controller report a `leave` that never really happened, so peek state
    // is re-derived from where the pointer actually is when the hold ends.
    // An "inside" answer is trustworthy at once; an "outside" one is not yet
    // (see GRAB_SETTLE_MS), so it is checked again once things settle.
    let inside = if take { None } else { Some(pointer_inside(window)) };
    let mut recheck = None;

    {
        let mut s = slide.borrow_mut();
        if take {
            s.held += 1;
        } else {
            s.held = s.held.saturating_sub(1);
            if s.held == 0 {
                s.settle_gen += 1;
                match inside {
                    Some(true) => s.peeking = true,
                    // Leave peeking as it stood before the grab — the dock is
                    // out, since it was being used — and decide shortly.
                    Some(false) => recheck = Some(s.settle_gen),
                    None => {}
                }
            }
        }
        let target = s.target();
        if (s.spring.target - target).abs() >= f64::EPSILON {
            s.spring.target = target;
            drop(s);
            animate_slide_on(slide, window, cfg);
        }
    }

    if let Some(generation) = recheck {
        let (slide, window, cfg) = (slide.clone(), window.clone(), cfg.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(GRAB_SETTLE_MS), move || {
            {
                let s = slide.borrow();
                // A newer hold or release has taken over; its check decides.
                if s.settle_gen != generation || s.held > 0 {
                    return;
                }
            }
            let inside = pointer_inside(&window);
            tracing::debug!(inside, "pointer after grab settled");
            surface_set_peeking(&slide, &window, &cfg, inside);
        });
    }
}

/// A launch pulse in progress.
#[derive(Debug, Clone, Copy)]
struct Pulse {
    /// Frame time of its first frame; 0 until the tick loop reaches it.
    start_us: i64,
    /// Breathe until the item has a window, rather than for one breath.
    until_window: bool,
}

/// The fill level `t` seconds into a pulse: full at the click, easing down to
/// the floor and back once per period — a sine ease-in-out, like the shell's.
fn pulse_level(t: f64) -> f64 {
    let wave = (1.0 + (std::f64::consts::TAU * t / PULSE_PERIOD_S).cos()) / 2.0;
    PULSE_FLOOR + (1.0 - PULSE_FLOOR) * wave
}

/// Start launch feedback on slot `index`.
fn pulse(state: &Rc<RefCell<State>>, index: usize) {
    {
        let mut s = state.borrow_mut();
        let Some(item) = s.data.get(index) else { return };
        // An app with nothing open waits for its window. Anything else — a
        // command tile, files handed to an app already running — has no
        // window to wait for, so it gets a single breath.
        let until_window = item.kind == ItemKind::App && item.windows.is_empty();
        if let Some(p) = s.pulses.get_mut(index) {
            *p = Some(Pulse { start_us: 0, until_window });
        }
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
/// Top-left of a slot's hover plate, in surface coordinates.
///
/// `GtkFixed` expresses a child's position *as* its transform, so the position
/// given to `put()` is discarded the first time `apply` runs. Both therefore
/// have to come from here, or the plate ends up offset from its icon by
/// exactly the margin — visibly off-centre, down and to the right.
fn plate_origin(geom: &Geometry, i: usize) -> (f64, f64) {
    let (x, y) = geom.slots[i];
    (x - PLATE_MARGIN, y - PLATE_MARGIN)
}

/// A small progress ring in the corner of a playing app's icon.
#[derive(Clone)]
struct MediaRing {
    area: gtk::DrawingArea,
    /// Fraction played, and whether it is playing (a paused track draws dim).
    value: Rc<Cell<(f64, bool)>>,
}

impl MediaRing {
    /// Show the player's state, or hide the ring when there is none.
    fn set(&self, player: Option<&crate::media::Player>) {
        match player {
            Some(p) => {
                // A player with no known length still gets a ring, drawn full:
                // it says "this is what is playing" even without progress.
                self.value.set((p.progress().unwrap_or(1.0), p.playing));
                self.area.set_visible(true);
                self.area.queue_draw();
            }
            None => self.area.set_visible(false),
        }
    }
}

fn media_ring(icon: i32) -> MediaRing {
    let d = (icon as f64 * 0.36).round() as i32;
    let area = gtk::DrawingArea::new();
    area.add_css_class("dock-media-ring");
    area.set_size_request(d, d);
    area.set_halign(gtk::Align::End);
    area.set_valign(gtk::Align::End);
    area.set_can_target(false);
    area.set_visible(false);

    let value = Rc::new(Cell::new((0.0, false)));
    {
        let value = value.clone();
        area.set_draw_func(move |a, cr, w, h| {
            let (progress, playing) = value.get();
            let (w, h) = (w as f64, h as f64);
            let line = (w * 0.16).max(2.0);
            let r = w.min(h) / 2.0 - line / 2.0;
            let (cx, cy) = (w / 2.0, h / 2.0);
            // The colour comes from CSS (the theme accent), so the ring
            // follows theme changes like everything else.
            let c = a.color();
            let alpha = if playing { 1.0 } else { 0.5 };

            // A dark disc underneath keeps the ring legible on any icon.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
            cr.arc(cx, cy, r + line / 2.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();

            cr.set_line_width(line);
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.25 * alpha);
            cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();

            if progress > 0.0 {
                let start = -std::f64::consts::FRAC_PI_2;
                cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, alpha);
                cr.arc(cx, cy, r, start, start + progress * std::f64::consts::TAU);
                let _ = cr.stroke();
            }
        });
    }
    MediaRing { area, value }
}

/// A turning arc on the Downloads stack while something downloads.
///
/// Turning rather than filling, because no progress is knowable: a browser's
/// partial file grows, but only the browser knows the size it will reach.
/// It redraws only while shown, so an idle dock still does nothing per frame.
#[derive(Clone)]
struct Spinner {
    area: gtk::DrawingArea,
    tick: Rc<RefCell<Option<gtk::TickCallbackId>>>,
}

/// One turn of the spinner.
const SPIN_PERIOD_S: f64 = 1.2;

impl Spinner {
    fn set(&self, active: bool) {
        let mut tick = self.tick.borrow_mut();
        if active && tick.is_none() {
            self.area.set_visible(true);
            *tick = Some(self.area.add_tick_callback(|a, _| {
                a.queue_draw();
                glib::ControlFlow::Continue
            }));
        } else if !active {
            if let Some(id) = tick.take() {
                id.remove();
            }
            self.area.set_visible(false);
        }
    }
}

fn spinner(icon: i32) -> Spinner {
    let d = (icon as f64 * 0.36).round() as i32;
    let area = gtk::DrawingArea::new();
    area.add_css_class("dock-download-ring");
    area.set_size_request(d, d);
    area.set_halign(gtk::Align::End);
    area.set_valign(gtk::Align::End);
    area.set_can_target(false);
    area.set_visible(false);
    area.set_draw_func(|a, cr, w, h| {
        let t = a.frame_clock().map(|c| c.frame_time()).unwrap_or(0) as f64 / 1_000_000.0;
        let (w, h) = (w as f64, h as f64);
        let line = (w * 0.16).max(2.0);
        let r = w.min(h) / 2.0 - line / 2.0;
        let (cx, cy) = (w / 2.0, h / 2.0);
        let c = a.color();
        let (red, green, blue) = (c.red() as f64, c.green() as f64, c.blue() as f64);

        // Drawn like the media ring, so the two read as one family.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        cr.arc(cx, cy, r + line / 2.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        cr.set_line_width(line);
        cr.set_source_rgba(red, green, blue, 0.25);
        cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();

        let start = (t / SPIN_PERIOD_S).fract() * std::f64::consts::TAU;
        cr.set_source_rgba(red, green, blue, 1.0);
        cr.arc(cx, cy, r, start, start + std::f64::consts::TAU * 0.3);
        let _ = cr.stroke();
    });
    Spinner { area, tick: Rc::new(RefCell::new(None)) }
}

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
    if item.kind == ItemKind::Workspace {
        // A number in a pill, like the bar's workspace widget. Occupancy and
        // the current workspace are CSS states rather than separate widgets,
        // so a refresh can move them without rebuilding anything.
        let label = gtk::Label::new(Some(&item.label));
        label.add_css_class("dock-workspace");
        set_class(&label, "occupied", !item.windows.is_empty());
        set_class(&label, "current", item.active);
        label.set_attributes(Some(&glyph_attrs((size as f64 * 0.62).round() as i32)));
        return label.upcast::<gtk::Widget>();
    }

    // A command tile has no icon at all, only a glyph, so it ignores the
    // glyph_ui preference — that setting is about whether the dock's *furniture*
    // is drawn in the bar's monochrome language, not about items that have
    // nothing else to draw.
    let always_glyph =
        matches!(item.kind, ItemKind::Command | ItemKind::Scratchpad | ItemKind::Recording);
    // A tray item often ships no themed icon, only raw pixels. Uploading them
    // as a texture is the only way to show such an app at all.
    if let Some(pixmap) = &item.pixmap {
        if item.icon.is_empty() {
            if let Some(img) = pixmap_image(pixmap, size) {
                return img.upcast::<gtk::Widget>();
            }
        }
    }

    match item.glyph.as_deref().filter(|_| cfg.items.glyph_ui || always_glyph) {
        Some(glyph) => {
            let w = glyph_widget(glyph, size);
            if item.kind == ItemKind::Recording {
                w.add_css_class("dock-recording");
            }
            w
        }
        None => make_icon(&item.icon, size).upcast::<gtk::Widget>(),
    }
}

/// A glyph drawn centred on its *ink*, not on its advance box.
///
/// Pango centres text by its logical extents, which is right for text and
/// wrong for an icon font: Nerd Font glyphs have asymmetric side bearings, so
/// the visible mark lands off-centre by a different amount for every glyph.
/// Measured on this machine that was a 5px spread between the Home and
/// Documents glyphs — plainly visible against the hover fill, which *is*
/// centred on the slot.
///
/// So the label is placed by hand inside a `gtk::Fixed`, offset by the
/// difference between its ink centre and its layout origin. Ink extents are
/// only known once the label has a Pango context, hence the deferral to `map`.
fn glyph_widget(glyph: &str, size: i32) -> gtk::Widget {
    let holder = gtk::Fixed::new();
    holder.set_size_request(size, size);

    let label = gtk::Label::new(Some(glyph));
    label.add_css_class("dock-glyph");
    // Glyphs are drawn by the font, so the size has to come from the type
    // scale rather than from a pixel-size request. The ratio leaves the same
    // optical weight as a themed icon of `size`.
    label.set_attributes(Some(&glyph_attrs(size)));
    holder.put(&label, 0.0, 0.0);

    {
        let holder = holder.clone();
        let label_ref = label.clone();
        // Re-centred on every map rather than once: a font or scale change
        // gives the same glyph different metrics, and a stale offset would be
        // worse than none.
        label.connect_map(move |_| centre_on_ink(&holder, &label_ref, size));
    }

    holder.upcast::<gtk::Widget>()
}

/// Move `label` within `holder` so the glyph's ink is centred on the slot.
fn centre_on_ink(holder: &gtk::Fixed, label: &gtk::Label, size: i32) {
    let (ink, _logical) = label.layout().extents();
    if ink.width() <= 0 || ink.height() <= 0 {
        return;
    }
    let scale = gtk::pango::SCALE as f64;
    let ink_cx = (ink.x() as f64 + ink.width() as f64 / 2.0) / scale;
    let ink_cy = (ink.y() as f64 + ink.height() as f64 / 2.0) / scale;
    let centre = size as f64 / 2.0;
    holder.move_(label, centre - ink_cx, centre - ink_cy);
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

/// Turn a StatusNotifierItem pixmap into an image.
///
/// The protocol specifies ARGB32 in network byte order, which is exactly
/// GDK's `A8r8g8b8` — no conversion, just a copy into a texture. Returns
/// `None` rather than panicking on a pixmap whose declared size does not match
/// its data, because that data comes from another application.
fn pixmap_image(pixmap: &(i32, i32, Vec<u8>), size: i32) -> Option<gtk::Image> {
    let (w, h, ref data) = *pixmap;
    let (w, h) = (w.max(0) as usize, h.max(0) as usize);
    let stride = w.checked_mul(4)?;
    if w == 0 || h == 0 || data.len() < stride.checked_mul(h)? {
        return None;
    }

    let bytes = glib::Bytes::from(&data[..stride * h]);
    let texture = gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::A8r8g8b8,
        &bytes,
        stride,
    );

    let img = gtk::Image::from_paintable(Some(&texture));
    img.set_pixel_size(size);
    img.add_css_class("dock-icon");
    Some(img)
}

fn make_icon(icon: &str, size: i32) -> gtk::Image {
    let img = gtk::Image::new();
    img.set_pixel_size(size);
    img.add_css_class("dock-icon");
    crate::ui::set_app_icon(&img, icon, size);
    img
}


// ── window previews ─────────────────────────────────────────────────────────

type MakePanel = Box<dyn FnOnce() -> Rc<Panel>>;

/// Window previews for one dock surface: when the strip opens, what it shows,
/// and holding the dock out while the pointer is over it.
struct Previews {
    /// Built on first hover rather than with the surface: the dock is rebuilt
    /// whenever an unpinned app opens or closes, and every strip opens a
    /// Wayland connection of its own for capturing.
    panel: RefCell<Option<Rc<Panel>>>,
    make: RefCell<Option<MakePanel>>,
    /// Set once capture turns out to be unavailable, so a compositor without
    /// it is not asked again on every hover.
    unavailable: Cell<bool>,
    /// Slot whose windows the strip was last shown for.
    showing: Cell<Option<usize>>,
    /// What those windows were, so an unchanged strip is not rebuilt.
    shown_tiles: RefCell<Vec<(u64, String, String)>>,
    /// Bumped on every hover change, so a stale show or hide timer does
    /// nothing.
    generation: Cell<u64>,
    delay_ms: u64,
    monitor: Option<gdk::Monitor>,
    /// Usable length along the dock's axis, read from Hyprland on first show.
    span: Cell<Option<f64>>,
}

impl Previews {
    fn new(
        app: &gtk::Application,
        monitor: Option<&gdk::Monitor>,
        state: &Rc<RefCell<State>>,
        sink: &ActionSink,
        slide: &Rc<RefCell<Slide>>,
        window: &gtk::ApplicationWindow,
        cfg: &Config,
    ) -> Rc<Self> {
        let monitor = monitor.cloned().or_else(first_monitor);
        let previews = Rc::new(Self {
            panel: RefCell::new(None),
            make: RefCell::new(None),
            unavailable: Cell::new(false),
            showing: Cell::new(None),
            shown_tiles: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            delay_ms: cfg.preview.delay_ms,
            monitor: monitor.clone(),
            span: Cell::new(None),
        });

        // Weak on both sides: the dock's state owns these previews, and the
        // strip's callbacks must not keep either alive.
        let (me, st) = (Rc::downgrade(&previews), Rc::downgrade(state));
        let (app, sink, slide, window, cfg) =
            (app.clone(), sink.clone(), slide.clone(), window.clone(), cfg.clone());
        let make = move || {
            // Whether the strip is holding the dock out, so enter and leave
            // can never release a hold they did not take, or take two.
            let holding = Rc::new(Cell::new(false));
            let on_focus: Rc<dyn Fn(crate::hypr::Address)> =
                Rc::new(move |address| sink(MenuAction::Command(DockCommand::Focus(address))));
            let on_enter: Rc<dyn Fn()> = {
                let (holding, slide, window, cfg, me) =
                    (holding.clone(), slide.clone(), window.clone(), cfg.clone(), me.clone());
                Rc::new(move || {
                    if let Some(p) = me.upgrade() {
                        p.bump();
                    }
                    // Over the strip, the pointer is off the dock's surface;
                    // without a hold the dock would hide from under it.
                    if !holding.replace(true) {
                        hold(&slide, &window, &cfg, true);
                    }
                })
            };
            let on_leave: Rc<dyn Fn()> = {
                let (slide, window, cfg) = (slide.clone(), window.clone(), cfg.clone());
                Rc::new(move || {
                    if holding.replace(false) {
                        hold(&slide, &window, &cfg, false);
                    }
                    if let (Some(p), Some(st)) = (me.upgrade(), st.upgrade()) {
                        let generation = p.bump();
                        p.hide_later(&st, generation);
                    }
                })
            };
            Panel::new(
                &app,
                monitor.as_ref(),
                cfg.dock.position,
                cfg.preview.width,
                on_focus,
                on_enter,
                on_leave,
            )
        };
        *previews.make.borrow_mut() = Some(Box::new(make));
        previews
    }

    fn bump(&self) -> u64 {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        generation
    }

    /// The strip, building it on first use. `None` when windows cannot be
    /// captured here.
    fn panel(&self) -> Option<Rc<Panel>> {
        if let Some(p) = self.current() {
            return Some(p);
        }
        if self.unavailable.get() {
            return None;
        }
        let make = self.make.borrow_mut().take()?;
        let panel = make();
        if !panel.available() {
            panel.close();
            self.unavailable.set(true);
            return None;
        }
        *self.panel.borrow_mut() = Some(panel.clone());
        Some(panel)
    }

    fn current(&self) -> Option<Rc<Panel>> {
        self.panel.borrow().clone()
    }

    /// The slot whose windows are on screen right now.
    fn shown(&self) -> Option<usize> {
        self.showing.get().filter(|_| self.current().is_some_and(|p| p.is_visible()))
    }

    /// Show slot `i`'s windows, or hide the strip if it no longer has any.
    fn show(&self, state: &Rc<RefCell<State>>, i: usize) {
        let plan = {
            let s = state.borrow();
            s.data
                .get(i)
                .filter(|d| d.kind == ItemKind::App && !d.windows.is_empty())
                .map(|item| (tiles_for(item), self.anchor(&s, i)))
        };
        let Some((tiles, anchor)) = plan else {
            self.hide();
            return;
        };
        let Some(anchor) = anchor else { return };
        let Some(panel) = self.panel() else { return };

        let key: Vec<(u64, String, String)> = tiles
            .iter()
            .map(|t| (t.address.as_u64(), t.title.clone(), t.workspace.clone()))
            .collect();
        if self.shown() == Some(i) && *self.shown_tiles.borrow() == key {
            return;
        }
        panel.show(&tiles, anchor);
        *self.shown_tiles.borrow_mut() = key;
        self.showing.set(Some(i));

        // The strip sits where the name label would, and names every window.
        let mut s = state.borrow_mut();
        s.tip_generation += 1;
        s.tip_label.set_visible(false);
    }

    /// Where the strip goes for slot `i`.
    ///
    /// Layer surfaces are placed within the monitor less whatever other
    /// surfaces reserve — the bar, mostly. The dock is centred in that area
    /// and the strip's margins count from its edges, so the icon's position is
    /// worked out in the same terms.
    fn anchor(&self, s: &State, i: usize) -> Option<preview::Anchor> {
        let g = &s.geom;
        let vertical = s.cfg.dock.position.is_vertical();
        let span = match self.span.get() {
            Some(span) => span,
            None => {
                let span = usable_span(self.monitor.as_ref(), vertical);
                self.span.set(Some(span));
                span
            }
        };
        let (sx, sy) = *g.slots.get(i)?;
        let extent = g.extents.get(i).copied().unwrap_or(s.cfg.dock.icon_size);
        let (slot, window_len) = if vertical { (sy, g.window_h) } else { (sx, g.window_w) };
        let along = (span - window_len) / 2.0 + slot + extent / 2.0;

        let mut from_edge = match s.cfg.dock.position {
            Position::Bottom => g.window_h - g.panel_y,
            Position::Top => g.panel_y + g.panel_h,
            Position::Left => g.panel_x + g.panel_w,
            Position::Right => g.window_w - g.panel_x,
        };
        // A dock that reserves its space has already taken that much off the
        // area the strip is placed in.
        if s.cfg.dock.reserve_space {
            from_edge -= if vertical { g.window_w } else { g.window_h };
        }
        Some(preview::Anchor { along, span, from_edge })
    }

    /// Hide the strip after a moment, unless by then the pointer has reached
    /// it or come back to its icon.
    fn hide_later(self: &Rc<Self>, state: &Rc<RefCell<State>>, generation: u64) {
        let (me, st) = (Rc::downgrade(self), Rc::downgrade(state));
        glib::timeout_add_local_once(std::time::Duration::from_millis(PREVIEW_LINGER_MS), move || {
            let (Some(me), Some(st)) = (me.upgrade(), st.upgrade()) else { return };
            if me.generation.get() != generation {
                return;
            }
            if me.current().is_some_and(|p| p.pointer_inside()) {
                return;
            }
            let hovered = st.borrow().hovered;
            if hovered.is_some() && hovered == me.showing.get() {
                return;
            }
            me.hide();
        });
    }

    /// Redraw an open strip from fresh data.
    fn refresh(&self, state: &Rc<RefCell<State>>) {
        if let Some(i) = self.shown() {
            self.show(state, i);
        }
    }

    fn hide(&self) {
        self.bump();
        self.showing.set(None);
        self.shown_tiles.borrow_mut().clear();
        if let Some(p) = self.current() {
            p.hide();
        }
    }

    fn close(&self) {
        self.make.borrow_mut().take();
        let panel = self.panel.borrow_mut().take();
        if let Some(p) = panel {
            p.close();
        }
    }
}

/// One preview tile per window, labelled with its title and workspace.
fn tiles_for(item: &DockItem) -> Vec<preview::Tile> {
    item.windows
        .iter()
        .enumerate()
        .map(|(k, address)| {
            let meta = item.window_meta.get(k);
            preview::Tile {
                address: address.clone(),
                title: meta
                    .map(|m| m.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| item.label.clone()),
                workspace: meta.map(|m| m.workspace_label().to_string()).unwrap_or_default(),
                icon: item.icon.clone(),
            }
        })
        .collect()
}

/// Open, move or hide the preview strip as hover moves to `hit`.
fn preview_hover(state: &Rc<RefCell<State>>, hit: Option<usize>) {
    let (previews, target) = {
        let s = state.borrow();
        let Some(p) = s.previews.clone() else { return };
        let target = hit.filter(|&i| {
            s.data.get(i).is_some_and(|d| d.kind == ItemKind::App && !d.windows.is_empty())
        });
        (p, target)
    };
    // Any pending show or hide belonged to the previous hover.
    let generation = previews.bump();
    match target {
        // Back on the icon already showing: the bump was all it took.
        Some(i) if previews.shown() == Some(i) => {}
        // Sweeping along the dock with the strip open follows at once, the
        // way a menu bar does once one menu is open.
        Some(i) if previews.shown().is_some() => previews.show(state, i),
        Some(i) => {
            let (me, st) = (Rc::downgrade(&previews), Rc::downgrade(state));
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(previews.delay_ms),
                move || {
                    let (Some(me), Some(st)) = (me.upgrade(), st.upgrade()) else { return };
                    if me.generation.get() != generation || st.borrow().hovered != Some(i) {
                        return;
                    }
                    me.show(&st, i);
                },
            );
        }
        None if previews.shown().is_some() => previews.hide_later(state, generation),
        None => {}
    }
}

/// Hide the strip at once, e.g. because its icon was clicked.
fn hide_previews(state: &Rc<RefCell<State>>) {
    let previews = state.borrow().previews.clone();
    if let Some(p) = previews {
        p.hide();
    }
}

fn first_monitor() -> Option<gdk::Monitor> {
    gdk::Display::default()?.monitors().item(0)?.downcast().ok()
}

/// Logical length of the area layer surfaces are placed in along the dock's
/// axis: the monitor, less what panels reserve at either end of it.
pub(crate) fn usable_span(monitor: Option<&gdk::Monitor>, vertical: bool) -> f64 {
    let Some(m) = monitor else { return if vertical { 1080.0 } else { 1920.0 } };
    let g = m.geometry();
    let full = if vertical { g.height() } else { g.width() } as f64;
    let name = m.connector();
    let [left, top, right, bottom] = reserved(name.as_deref()).unwrap_or_default();
    let taken = if vertical { top + bottom } else { left + right };
    (full - taken).max(0.0)
}

/// What Hyprland reports as reserved on a monitor: left, top, right, bottom,
/// in logical pixels.
fn reserved(monitor: Option<&str>) -> Option<[f64; 4]> {
    let out = std::process::Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let m = json.as_array()?.iter().find(|m| {
        monitor.is_none_or(|name| m.get("name").and_then(|n| n.as_str()) == Some(name))
    })?;
    let r = m.get("reserved")?.as_array()?;
    let at = |i: usize| r.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
    Some([at(0), at(1), at(2), at(3)])
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
    hover_to(state, hit, true);
}

/// Move hover to `hit`, opening window previews for it when `previews`.
fn hover_to(state: &Rc<RefCell<State>>, hit: Option<usize>, previews: bool) {
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
    // After the early return above: motion within one icon must not restart
    // the preview delay on every event.
    if previews {
        preview_hover(state, hit);
    } else {
        hide_previews(state);
    }

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
        // The strip already names every window, and sits where the label
        // would.
        if s.previews.as_ref().is_some_and(|p| p.shown().is_some()) {
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
            if let Some(mut p) = s.pulses[i] {
                if p.start_us == 0 {
                    p.start_us = now;
                }
                let t = (now - p.start_us) as f64 / 1_000_000.0;
                let over = t >= PULSE_TIMEOUT_S
                    || if p.until_window {
                        s.data.get(i).is_none_or(|d| !d.windows.is_empty())
                    } else {
                        t >= PULSE_PERIOD_S
                    };
                if over {
                    // Hand the level to the hover spring, so the fill eases to
                    // wherever hover wants it instead of snapping there.
                    s.hovers[i].pos = s.pulse_levels[i];
                    s.hovers[i].vel = 0.0;
                    s.pulses[i] = None;
                } else {
                    s.pulse_levels[i] = pulse_level(t);
                    s.pulses[i] = Some(p);
                }
                moving = true;
            }

            let zoom_busy = !s.springs[i].settled();
            let shift_busy = !s.shifts[i].settled();
            let hover_busy = !s.hovers[i].settled();
            if !zoom_busy && !shift_busy && !hover_busy {
                if s.pulses[i].is_some() {
                    s.apply(i);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slide() -> Slide {
        Slide {
            spring: Spring::at(0.0),
            hidden: false,
            peeking: false,
            held: 0,
            generation: 0,
            settle_gen: 0,
            away: false,
            edge: Edge::Bottom,
            base_margin: 0,
            travel: 76.0,
            extent: 80.0,
            ticking: false,
            last_us: 0,
        }
    }

    #[test]
    fn a_hidden_dock_keeps_its_sliver_and_peeks() {
        let mut s = slide();
        s.hidden = true;
        assert_eq!(s.target(), 76.0);
        s.peeking = true;
        assert_eq!(s.target(), 0.0);
    }

    #[test]
    fn away_leaves_the_screen_whatever_the_pointer_does() {
        let mut s = slide();
        s.away = true;
        s.peeking = true;
        s.held = 1;
        assert_eq!(s.target(), 80.0);
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn ws(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scrolling_steps_through_the_strip_and_wraps() {
        let names = ws(&["1", "2", "3", "4", "5"]);
        assert_eq!(scrolled_workspace(&names, Some(1), 1).as_deref(), Some("3"));
        assert_eq!(scrolled_workspace(&names, Some(1), -1).as_deref(), Some("1"));
        assert_eq!(scrolled_workspace(&names, Some(4), 1).as_deref(), Some("1"));
        assert_eq!(scrolled_workspace(&names, Some(0), -1).as_deref(), Some("5"));
        // A fast flick moves several.
        assert_eq!(scrolled_workspace(&names, Some(0), 2).as_deref(), Some("3"));
    }

    #[test]
    fn scrolling_from_off_the_strip_starts_at_an_end() {
        // The scratchpad, or a workspace past the fixed row, is in front.
        let names = ws(&["1", "2", "3"]);
        assert_eq!(scrolled_workspace(&names, None, 1).as_deref(), Some("1"));
        assert_eq!(scrolled_workspace(&names, None, -1).as_deref(), Some("3"));
    }

    #[test]
    fn less_than_a_step_does_nothing() {
        assert_eq!(scrolled_workspace(&ws(&["1", "2"]), Some(0), 0), None);
        assert_eq!(scrolled_workspace(&[], Some(0), 1), None);
    }

    #[test]
    fn a_pulse_starts_full_so_the_click_is_acknowledged_at_once() {
        assert!(close(pulse_level(0.0), 1.0));
    }

    #[test]
    fn a_pulse_dims_to_its_floor_mid_breath_and_comes_back() {
        assert!(close(pulse_level(PULSE_PERIOD_S / 2.0), PULSE_FLOOR));
        assert!(close(pulse_level(PULSE_PERIOD_S), 1.0));
    }

    #[test]
    fn a_pulse_never_leaves_its_range() {
        for k in 0..=1000 {
            let level = pulse_level(k as f64 * PULSE_TIMEOUT_S / 1000.0);
            assert!((PULSE_FLOOR - 1e-9..=1.0 + 1e-9).contains(&level), "{level}");
        }
    }
}
