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

use std::cell::RefCell;
use std::rc::Rc;

use crate::anim::Spring;
use crate::config::{Config, Position};
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

struct State {
    fixed: gtk::Fixed,
    /// Current item data. Click handlers read this by index rather than
    /// capturing a copy, so an in-place refresh cannot leave them stale.
    data: Vec<DockItem>,
    /// Per-slot indicator and badge widgets. Always created, shown or hidden
    /// as state changes, so a refresh never has to build widgets.
    indicators: Vec<gtk::Widget>,
    badges: Vec<gtk::Label>,
    /// The slot widgets, needed to anchor the name label over the right icon.
    slots: Vec<gtk::Widget>,
    /// The hovered icon's name, drawn in the reserved band at the top of the
    /// surface. GTK's own tooltips follow the pointer, which puts the name
    /// below the icon and over the panel; a dock wants it above the icon.
    tip_label: gtk::Label,
    /// Bumped on every hover change so a late tooltip timer is discarded.
    tip_generation: u64,
    tooltip_delay: u64,
    items: Vec<gtk::Widget>,
    springs: Vec<Spring>,
    /// Displacement away from the screen edge, in pixels, for launch bounce
    /// and urgency. Separate from the zoom spring so a bounce can play while
    /// the icon is magnified.
    bounces: Vec<Spring>,
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
        let zoom = self.cfg.magnify.zoom;
        // 0..1 as the spring travels from rest to full zoom.
        let p = if zoom > 1.0 { ((s.pos - 1.0) / (zoom - 1.0)).clamp(0.0, 1.0) } else { 0.0 };
        // Bounce rides on top of magnification lift, along the same axis.
        let lift = self.cfg.magnify.lift * p + bounce;
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
                // Keep the per-slot vectors aligned with the item list.
                let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                dot.set_visible(false);
                indicators.push(dot.upcast::<gtk::Widget>());
                let badge = gtk::Label::new(None);
                badge.set_visible(false);
                badges.push(badge);
                continue;
            }

            slot.set_size_request(size, size);

            let img = make_icon(&item.icon, size);
            slot.set_child(Some(&img));

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

        let state = Rc::new(RefCell::new(State {
            fixed: fixed.clone(),
            springs: vec![Spring::at(1.0); widget_count],
            bounces: vec![Spring::at(0.0); widget_count],
            data: items.to_vec(),
            indicators,
            badges,
            slots: slots.iter().cloned().map(|s| s.upcast::<gtk::Widget>()).collect(),
            tip_label: tip_label.clone(),
            tip_generation: 0,
            tooltip_delay: cfg.dock.tooltip_delay_ms,
            items: widgets,
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

        let edge = init_layer_shell(&window, cfg, monitor);
        window.present();

        let slide = Rc::new(RefCell::new(Slide {
            spring: Spring::at(0.0),
            hidden: false,
            peeking: false,
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
            if items.get(i).is_some_and(|it| it.interactive()) {
                attach_clicks(slot, &sink, &state, i);
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
            let target = if s.hidden && !s.peeking { s.travel } else { 0.0 };
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
                        if slide.borrow().generation != gen {
                            return;
                        }
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
fn attach_clicks(
    slot: &gtk::Overlay,
    sink: &ActionSink,
    state: &Rc<RefCell<State>>,
    index: usize,
) {
    // ── left button ─────────────────────────────────────────────────────────
    let left = gtk::GestureClick::new();
    left.set_button(gdk::BUTTON_PRIMARY);
    {
        let sink = sink.clone();
        let state = state.clone();
        let anchor_left = slot.clone();
        left.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);

            // Read current data: focus may have moved since the dock was built.
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
                    pop.set_position(gtk::PositionType::Top);
                    pop.connect_closed(|p| p.unparent());
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
        right.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let item = {
                let s = state.borrow();
                match s.data.get(index) {
                    Some(i) => i.clone(),
                    None => return,
                }
            };
            let sink = sink.clone();
            let popover = if item.kind == ItemKind::Trash || item.kind == ItemKind::Folder {
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
            popover.set_position(gtk::PositionType::Top);
            // Detach on close so repeated right-clicks do not stack popovers
            // as children of the slot.
            popover.connect_closed(|p| p.unparent());
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
        let target = if s.hidden && !s.peeking { s.travel } else { 0.0 };
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
            if !zoom_busy && !bounce_busy {
                continue;
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
