//! Phase-0 de-risking spike: single-icon hover magnification.
//!
//! Validates the *feel* before committing to the real dock's widget tree.
//! Only the icon directly under the cursor scales — no neighbour wave.
//!
//! Two decisions carry over to production:
//!
//! 1. **Input and animation are decoupled.** Hover is hit-tested against
//!    static slot rectangles, never against the scaled icon. Hit-testing the
//!    growing icon makes it capture the cursor as it expands, which oscillates
//!    (grow -> cursor now inside -> stay grown -> ...) and reads as jitter.
//!
//! 2. **Scaling is a pure `gsk::Transform`,** anchored at each slot's
//!    bottom-centre so icons rise into the headroom above the panel. No
//!    relayout happens per frame; the compositor does the work.
//!
//! The tick callback is installed only while something is actually moving, so
//! an idle dock costs zero CPU.
//!
//! Tunable live, without recompiling:
//!   SPIKE_ZOOM=1.6 SPIKE_STIFF=520 SPIKE_DAMP=30 SPIKE_LIFT=10 \
//!       cargo run --bin spike-magnify

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

use std::cell::RefCell;
use std::rc::Rc;

// ── geometry ────────────────────────────────────────────────────────────────
const ICON: f64 = 48.0;
const PAD_X: f64 = 16.0;
const PAD_Y: f64 = 10.0;
const PANEL_H: f64 = ICON + PAD_Y * 2.0;
/// Vertical room above the panel for a magnified icon to grow into.
const HEADROOM: f64 = 52.0;

/// Read an f64 from the environment, falling back to a default.
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Tuning knobs, resolved once at startup.
#[derive(Clone, Copy)]
struct Tuning {
    zoom: f64,
    /// Spring constant. Higher = snappier.
    stiffness: f64,
    /// Critical damping is `2*sqrt(stiffness)`; below that gives a subtle
    /// overshoot, which is what reads as "pop" rather than "slide".
    damping: f64,
    /// Extra upward translation at full zoom, on top of bottom-anchored scaling.
    lift: f64,
}

impl Tuning {
    fn from_env() -> Self {
        let stiffness = env_f64("SPIKE_STIFF", 460.0);
        Self {
            zoom: env_f64("SPIKE_ZOOM", 1.45),
            stiffness,
            // ~0.82 of critical: lively, but settles without visible ringing.
            damping: env_f64("SPIKE_DAMP", 2.0 * stiffness.sqrt() * 0.82),
            lift: env_f64("SPIKE_LIFT", 6.0),
        }
    }

    /// Slot pitch. Keeping spacing >= the icon's horizontal growth means a
    /// magnified icon never overlaps its neighbours, so draw order is moot and
    /// neighbours need not move.
    fn spacing(&self) -> f64 {
        ((self.zoom - 1.0) * ICON).max(10.0) + 2.0
    }
}

// ── spring ──────────────────────────────────────────────────────────────────
/// Damped harmonic oscillator, integrated semi-implicitly.
///
/// Chosen over a fixed-duration easing curve because it is interruptible: when
/// the cursor leaves early the icon reverses from wherever it is, carrying its
/// velocity, instead of snapping or restarting a tween.
#[derive(Clone, Copy)]
struct Spring {
    pos: f64,
    vel: f64,
    target: f64,
}

impl Spring {
    fn at(v: f64) -> Self {
        Self { pos: v, vel: 0.0, target: v }
    }

    fn step(&mut self, dt: f64, t: &Tuning) {
        let accel = t.stiffness * (self.target - self.pos) - t.damping * self.vel;
        self.vel += accel * dt;
        self.pos += self.vel * dt;
    }

    fn settled(&self) -> bool {
        (self.target - self.pos).abs() < 0.0005 && self.vel.abs() < 0.0005
    }

    /// Snap exactly onto the target so a settled icon renders identically
    /// every frame and we can safely stop ticking.
    fn settle(&mut self) {
        self.pos = self.target;
        self.vel = 0.0;
    }
}

// ── app state ───────────────────────────────────────────────────────────────
struct Dock {
    fixed: gtk::Fixed,
    icons: Vec<gtk::Widget>,
    /// Slot origins, in `fixed` coordinates. `GtkFixed` expresses a child's
    /// position *as* its child transform, so `set_child_transform` replaces
    /// the placement from `put()`. Every transform we build must therefore
    /// re-apply the slot origin itself, or the icon teleports to (0, 0).
    slots: Vec<(f64, f64)>,
    springs: Vec<Spring>,
    tuning: Tuning,
    /// Index of the slot under the cursor, if any.
    hovered: Option<usize>,
    /// `frame_time()` of the previous tick, in microseconds.
    last_us: i64,
    /// Set while a tick callback is installed, so we never install two.
    ticking: bool,
    // FPS accounting.
    frames: u32,
    fps_since_us: i64,
    fps_label: gtk::Label,
    /// Per-frame deltas (ms) for one animation run, summarised on settle.
    dts: Vec<f64>,
}

impl Dock {
    /// Slot placement, then bottom-centre-anchored scale plus lift, as a
    /// single GPU transform.
    fn transform_for(&self, i: usize) -> gsk::Transform {
        let spring = &self.springs[i];
        let (sx, sy) = self.slots[i];
        let t = self.tuning;
        // `p` runs 0..1 as the spring travels 1.0 -> zoom.
        let p = ((spring.pos - 1.0) / (t.zoom - 1.0)).clamp(0.0, 1.0);
        let k = spring.pos as f32;
        let anchor = graphene::Point::new(ICON as f32 / 2.0, ICON as f32);

        gsk::Transform::new()
            // Slot origin first — this is what `put()` would otherwise supply.
            .translate(&graphene::Point::new(sx as f32, sy as f32))
            .translate(&graphene::Point::new(0.0, -(t.lift * p) as f32))
            .translate(&anchor)
            .scale(k, k)
            .translate(&graphene::Point::new(-anchor.x(), -anchor.y()))
    }

    fn apply(&self, i: usize) {
        let tf = self.transform_for(i);
        self.fixed.set_child_transform(&self.icons[i], Some(&tf));
    }

    /// Point the springs at the current hover state.
    fn retarget(&mut self) {
        for (i, s) in self.springs.iter_mut().enumerate() {
            s.target = if Some(i) == self.hovered { self.tuning.zoom } else { 1.0 };
        }
    }
}

/// Report the frame-interval distribution for one animation run.
fn summarise(mut v: Vec<f64>) {
    if v.len() < 3 {
        return;
    }
    // Drop the first sample: it spans tick-installation to first frame, not a
    // frame-to-frame interval.
    v.remove(0);
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let med = v[n / 2];
    let p95 = v[((n as f64 * 0.95) as usize).min(n - 1)];
    let long = v.iter().filter(|d| **d > 20.0).count();
    eprintln!(
        "frames={n} median={med:.2}ms p95={p95:.2}ms max={:.2}ms over-20ms={long} => {:.0} fps median",
        v[n - 1],
        1000.0 / med
    );
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("dev.omarchy.DockSpike")
        .build();

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("../../resources/spike.css"));
        if let Some(d) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &d,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    app.connect_activate(build);
    app.run()
}

/// Pick the first icon name the active theme actually has, so the spike shows
/// real artwork instead of a row of "missing image" glyphs.
fn resolve_icon(candidates: &[&str]) -> String {
    let theme = gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d))
        .expect("no display");

    let picked = candidates
        .iter()
        .find(|n| theme.has_icon(n))
        .map(|n| n.to_string())
        .unwrap_or_else(|| "application-x-executable".into());
    picked
}

fn build(app: &gtk::Application) {
    let tuning = Tuning::from_env();
    let spacing = tuning.spacing();

    // Roughly the user's real pinned set, so the spike looks like the product.
    let wanted: Vec<String> = [
        &["chromium", "google-chrome", "web-browser"][..],
        &["microsoft-edge", "microsoft-edge-dev", "web-browser"][..],
        &["code", "vscode", "com.visualstudio.code", "text-editor"][..],
        &["spotify", "com.spotify.Client", "multimedia-player"][..],
        &["org.gnome.Nautilus", "system-file-manager", "folder"][..],
        &["alacritty", "utilities-terminal", "terminal"][..],
        &["org.gnome.Settings", "preferences-system"][..],
        &["user-trash", "user-trash-full"][..],
    ]
    .iter()
    .map(|c| resolve_icon(c))
    .collect();

    let n = wanted.len() as f64;
    let width = PAD_X * 2.0 + n * ICON + (n - 1.0) * spacing;
    let height = HEADROOM + PANEL_H;

    let fixed = gtk::Fixed::new();
    fixed.set_size_request(width as i32, height as i32);

    // Panel slab first, so icons draw on top of it.
    let panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    panel.add_css_class("dock-panel");
    panel.set_size_request(width as i32, PANEL_H as i32);
    fixed.put(&panel, 0.0, HEADROOM);

    // Slots are laid out once and never move; only transforms change.
    let mut icons = Vec::new();
    let mut slots = Vec::new();
    for (i, name) in wanted.iter().enumerate() {
        let img = gtk::Image::from_icon_name(name);
        img.set_pixel_size(ICON as i32);
        img.set_size_request(ICON as i32, ICON as i32);
        img.add_css_class("dock-icon");

        let x = PAD_X + i as f64 * (ICON + spacing);
        let y = HEADROOM + PAD_Y;
        fixed.put(&img, x, y);
        slots.push((x, y));
        icons.push(img.upcast::<gtk::Widget>());
    }

    let fps_label = gtk::Label::new(Some("idle"));
    fps_label.add_css_class("fps");
    fps_label.set_halign(gtk::Align::Start);
    fixed.put(&fps_label, 6.0, 6.0);

    let dock = Rc::new(RefCell::new(Dock {
        fixed: fixed.clone(),
        springs: vec![Spring::at(1.0); icons.len()],
        icons,
        slots,
        tuning,
        hovered: None,
        last_us: 0,
        ticking: false,
        frames: 0,
        fps_since_us: 0,
        fps_label,
        dts: Vec::new(),
    }));

    // ── hover: hit-test static slots, never the scaled icons ────────────────
    let motion = gtk::EventControllerMotion::new();
    {
        let dock = dock.clone();
        motion.connect_motion(move |_, x, y| {
            if std::env::var("SPIKE_DEBUG").is_ok() {
                eprintln!("motion {x:.0},{y:.0}");
            }
            // Only the panel band counts as hover. A magnified icon extends up
            // into the headroom, but the cursor that summoned it is still down
            // on the panel, so this stays stable while the icon grows.
            let hit = if y >= HEADROOM {
                let rel = x - PAD_X;
                let pitch = ICON + spacing;
                if rel < 0.0 {
                    None
                } else {
                    let idx = (rel / pitch).floor() as usize;
                    // Reject the gap between slots.
                    let within = rel - idx as f64 * pitch;
                    let count = dock.borrow().icons.len();
                    (within <= ICON && idx < count).then_some(idx)
                }
            } else {
                None
            };

            set_hover(&dock, hit);
        });
    }
    {
        let dock = dock.clone();
        motion.connect_leave(move |_| set_hover(&dock, None));
    }
    fixed.add_controller(motion);

    // ── window ──────────────────────────────────────────────────────────────
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .child(&fixed)
        .build();

    if std::env::var("SPIKE_NO_LAYER").is_err() {
        window.init_layer_shell();
        window.set_namespace(Some("omarchy-dock"));
        window.set_layer(Layer::Top);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 12);
        window.set_exclusive_zone(0);
    } else {
        eprintln!("spike: layer-shell DISABLED (normal window)");
    }
    // The dock must receive pointer motion without stealing keyboard focus.
    window.present();

    // Deterministic verification hook: pin a slot as hovered at startup so a
    // screenshot can assert the magnified geometry without driving the real
    // pointer around.
    if let Ok(i) = std::env::var("SPIKE_FORCE_HOVER") {
        if let Ok(i) = i.parse::<usize>() {
            let dock_ref = dock.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                set_hover(&dock_ref, Some(i));
            });
        }
    }

    // Bench mode: cycle the hovered slot so frame timing is measured over a
    // sustained run rather than a single 300ms settle.
    if std::env::var("SPIKE_BENCH").is_ok() {
        let dock_ref = dock.clone();
        let n = dock.borrow().icons.len();
        let step = std::cell::Cell::new(0usize);
        glib::timeout_add_local(std::time::Duration::from_millis(220), move || {
            let i = step.get();
            step.set(i + 1);
            if i >= 24 {
                summarise(std::mem::take(&mut dock_ref.borrow_mut().dts));
                std::process::exit(0);
            }
            set_hover(&dock_ref, Some(i % n));
            glib::ControlFlow::Continue
        });
    }

    eprintln!(
        "spike: zoom={:.2} stiffness={:.0} damping={:.1} lift={:.1} spacing={:.1}",
        tuning.zoom, tuning.stiffness, tuning.damping, tuning.lift, spacing
    );
}

/// Update hover state and make sure the animation loop is running.
fn set_hover(dock: &Rc<RefCell<Dock>>, hit: Option<usize>) {
    {
        let mut d = dock.borrow_mut();
        if d.hovered == hit {
            return;
        }
        d.hovered = hit;
        d.retarget();
    }
    ensure_ticking(dock);
}

/// Install a frame-clock callback if one is not already running.
///
/// The callback removes itself once every spring has settled, so a dock that
/// nobody is pointing at does no per-frame work at all.
fn ensure_ticking(dock: &Rc<RefCell<Dock>>) {
    {
        let mut d = dock.borrow_mut();
        if d.ticking {
            return;
        }
        d.ticking = true;
        d.last_us = 0;
        d.frames = 0;
        d.fps_since_us = 0;
    }

    let widget = dock.borrow().fixed.clone();
    let dock = dock.clone();

    widget.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut d = dock.borrow_mut();

        // First frame establishes the time base only.
        if d.last_us == 0 {
            d.last_us = now;
            d.fps_since_us = now;
            return glib::ControlFlow::Continue;
        }

        // Clamp dt: a long stall (compositor hiccup, laptop resume) would
        // otherwise blow the integrator up.
        let raw_dt = (now - d.last_us) as f64 / 1_000_000.0;
        d.dts.push(raw_dt * 1000.0);
        let dt = raw_dt.clamp(0.0, 1.0 / 30.0);
        d.last_us = now;

        let tuning = d.tuning;
        let mut moving = false;
        for i in 0..d.springs.len() {
            if d.springs[i].settled() {
                continue;
            }
            d.springs[i].step(dt, &tuning);
            if d.springs[i].settled() {
                d.springs[i].settle();
            } else {
                moving = true;
            }
            d.apply(i);
        }

        // FPS readout, refreshed twice a second.
        d.frames += 1;
        let elapsed = (now - d.fps_since_us) as f64 / 1_000_000.0;
        if elapsed >= 0.5 {
            let fps = d.frames as f64 / elapsed;
            d.fps_label.set_text(&format!("{fps:.0} fps"));
            d.frames = 0;
            d.fps_since_us = now;
        }

        if moving {
            glib::ControlFlow::Continue
        } else {
            d.ticking = false;
            d.fps_label.set_text("idle");
            summarise(std::mem::take(&mut d.dts));
            glib::ControlFlow::Break
        }
    });
}
