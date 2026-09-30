//! Window previews: a strip of live thumbnails above a hovered app.
//!
//! One tile per window — thumbnail, title, workspace — so an app open on
//! several workspaces can be told apart and jumped to directly. Clicking a
//! tile focuses that window, switching workspace if it has to.
//!
//! The strip is a layer surface of its own rather than a popover. A popover
//! either stays inside the dock's surface, which is far too small to hold it,
//! or grabs the pointer, which breaks hovering from one icon to the next. As
//! its own surface it can sit above the dock at any size, and pointer
//! movement between the two works as it does between any two windows.
//!
//! This module only renders and positions. When to show and hide is the
//! dock's decision, since it owns hover; the dock passes in what to show and
//! where, plus callbacks for focus and for holding itself out while the
//! pointer is over the strip.

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::capture::{Capturer, Frame, Request};
use crate::config::Position;
use crate::hypr::Address;

/// Layer namespace, so Hyprland rules can target the strip separately.
pub const NAMESPACE: &str = "omarchy-dock-preview";

/// Most tiles shown at once. An app with more windows than this is already
/// better served by its workspace strip than by a row of postage stamps.
const MAX_TILES: usize = 8;

/// Gap between the dock's panel and the strip.
const GAP: i32 = 8;

/// Thumbnails kept for the next hover. Past this the cache starts over, so
/// windows closed long ago are not held forever.
const CACHE_LIMIT: usize = 48;

/// One window to show.
#[derive(Debug, Clone)]
pub struct Tile {
    pub address: Address,
    pub title: String,
    /// Short workspace label: "3", or "scratchpad".
    pub workspace: String,
    /// Icon shown until the thumbnail arrives.
    pub icon: String,
    /// Minimized: drawn dimmed, and clicking restores it.
    pub minimized: bool,
    /// What clicking the tile sends.
    pub open: crate::runtime::DockCommand,
}

/// Where the strip goes, in logical pixels of the area layer surfaces are
/// placed in — the monitor minus whatever the bar reserves.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// Centre of the hovered icon along the dock's long axis.
    pub along: f64,
    /// Length of that area along the same axis, to keep the strip on screen.
    pub span: f64,
    /// Distance from the dock's screen edge to the far side of its panel.
    pub from_edge: f64,
}

pub struct Panel {
    me: Weak<Self>,
    window: gtk::Window,
    row: gtk::Box,
    position: Position,
    monitor: Option<gdk::Monitor>,
    tile_width: i32,
    capturer: Option<Capturer>,
    /// Bumped on every show, so a thumbnail for an earlier hover is dropped.
    token: Cell<u64>,
    /// Pictures awaiting a thumbnail, by window address.
    pictures: RefCell<HashMap<u64, (gtk::Picture, gtk::Image)>>,
    /// Last thumbnail per window, shown at once on the next hover while a
    /// fresh one is captured.
    cache: RefCell<HashMap<u64, gdk::MemoryTexture>>,
    pointer_inside: Cell<bool>,
    on_open: Rc<dyn Fn(crate::runtime::DockCommand)>,
    on_leave: Rc<dyn Fn()>,
}

impl Panel {
    /// Build the (hidden) strip. `on_enter` / `on_leave` fire as the pointer
    /// crosses into and out of it, so the dock can hold itself out meanwhile.
    pub fn new(
        app: &gtk::Application,
        monitor: Option<&gdk::Monitor>,
        position: Position,
        tile_width: f64,
        on_open: Rc<dyn Fn(crate::runtime::DockCommand)>,
        on_enter: Rc<dyn Fn()>,
        on_leave: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let row = gtk::Box::new(
            if position.is_vertical() { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal },
            6,
        );
        row.add_css_class("dock-preview");

        let window = gtk::Window::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .child(&row)
            .build();
        window.add_css_class("dock-preview-window");
        window.init_layer_shell();
        window.set_namespace(Some(NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        if let Some(m) = monitor {
            window.set_monitor(Some(m));
        }
        let (edge, cross) = edges(position);
        window.set_anchor(edge, true);
        window.set_anchor(cross, true);

        let (tx, rx) = async_channel::unbounded::<Frame>();
        let capturer = Capturer::spawn(tx);

        let panel = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            window,
            row,
            position,
            monitor: monitor.cloned(),
            tile_width: tile_width.round() as i32,
            capturer,
            token: Cell::new(0),
            pictures: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            pointer_inside: Cell::new(false),
            on_open,
            on_leave: on_leave.clone(),
        });

        // Thumbnails arrive from the capture thread.
        {
            let weak = Rc::downgrade(&panel);
            glib::spawn_future_local(async move {
                while let Ok(frame) = rx.recv().await {
                    let Some(panel) = weak.upgrade() else { break };
                    panel.accept(frame);
                }
            });
        }

        {
            let motion = gtk::EventControllerMotion::new();
            let (a, b) = (Rc::downgrade(&panel), Rc::downgrade(&panel));
            motion.connect_enter(move |_, _, _| {
                if let Some(p) = a.upgrade() {
                    p.pointer_inside.set(true);
                }
                on_enter();
            });
            // Only a leave that follows an enter counts: `hide` reports the
            // leave itself, and unmapping may report it again.
            motion.connect_leave(move |_| {
                if b.upgrade().is_some_and(|p| p.pointer_inside.replace(false)) {
                    on_leave();
                }
            });
            panel.window.add_controller(motion);
        }

        panel
    }

    /// Whether previews can work at all — i.e. whether the compositor lets
    /// windows be captured.
    pub fn available(&self) -> bool {
        self.capturer.is_some()
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn pointer_inside(&self) -> bool {
        self.pointer_inside.get()
    }

    /// Hide the strip. A pointer that was over it counts as having left, so
    /// whatever the dock held out on its behalf is let go.
    pub fn hide(&self) {
        self.window.set_visible(false);
        self.pictures.borrow_mut().clear();
        if self.pointer_inside.replace(false) {
            (self.on_leave)();
        }
    }

    /// Show `tiles` centred on `anchor`, requesting fresh thumbnails.
    pub fn show(&self, tiles: &[Tile], anchor: Anchor) {
        let token = self.token.get() + 1;
        self.token.set(token);

        while let Some(child) = self.row.first_child() {
            self.row.remove(&child);
        }
        self.pictures.borrow_mut().clear();

        let tiles = &tiles[..tiles.len().min(MAX_TILES)];
        let thumb_h = (self.tile_width as f64 * 0.62).round() as i32;
        let scale = self.monitor.as_ref().map(|m| m.scale()).unwrap_or(1.0);

        for tile in tiles {
            self.row.append(&self.tile(tile, thumb_h));
            if let Some(c) = &self.capturer {
                c.request(Request {
                    address: tile.address.as_u64(),
                    max_w: (self.tile_width as f64 * scale).round() as u32,
                    max_h: (thumb_h as f64 * scale).round() as u32,
                    token,
                });
            }
        }

        self.place(tiles.len(), thumb_h, anchor);
        self.window.set_visible(true);
    }

    fn tile(&self, tile: &Tile, thumb_h: i32) -> gtk::Widget {
        let button = gtk::Button::new();
        button.add_css_class("dock-preview-tile");
        if tile.minimized {
            button.add_css_class("minimized");
        }
        button.set_has_frame(false);
        button.set_tooltip_text(Some(&tile.title));

        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);

        // The app's icon sits in the thumbnail's place until the capture
        // lands, so the strip is never a row of blank boxes.
        let stack = gtk::Overlay::new();
        stack.set_size_request(self.tile_width, thumb_h);
        let icon = gtk::Image::new();
        crate::ui::set_app_icon(&icon, &tile.icon, thumb_h / 2);
        icon.set_pixel_size(thumb_h / 2);
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        stack.set_child(Some(&icon));
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.add_css_class("dock-preview-thumb");
        stack.add_overlay(&picture);

        let key = tile.address.as_u64();
        match self.cache.borrow().get(&key) {
            Some(tex) => {
                picture.set_paintable(Some(tex));
                icon.set_visible(false);
            }
            None => picture.set_visible(false),
        }
        self.pictures.borrow_mut().insert(key, (picture, icon));
        body.append(&stack);

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let title = gtk::Label::new(Some(&tile.title));
        title.add_css_class("dock-preview-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(1); // let the tile width decide
        footer.append(&title);
        if !tile.workspace.is_empty() {
            let ws = gtk::Label::new(Some(&tile.workspace));
            ws.add_css_class("dock-preview-workspace");
            footer.append(&ws);
        }
        body.append(&footer);
        button.set_child(Some(&body));

        let (open, on_open, me) = (tile.open.clone(), self.on_open.clone(), self.me.clone());
        button.connect_clicked(move |_| {
            on_open(open.clone());
            if let Some(panel) = me.upgrade() {
                panel.hide();
            }
        });
        button.upcast()
    }

    /// Put the strip beside the dock, centred on the hovered icon and kept on
    /// screen.
    fn place(&self, count: usize, thumb_h: i32, anchor: Anchor) {
        let n = count.max(1) as i32;
        // Tile + its padding, then the strip's own padding. Worked out rather
        // than measured, because the anchor margins must be set before the
        // surface maps, when nothing has been measured yet.
        let (tile_w, tile_h) = (self.tile_width + 16, thumb_h + 44);
        let (w, h) = if self.position.is_vertical() {
            (tile_w + 16, n * tile_h + (n - 1) * 6 + 16)
        } else {
            (n * tile_w + (n - 1) * 6 + 16, tile_h + 16)
        };

        let (edge, cross) = edges(self.position);
        let length = if self.position.is_vertical() { h } else { w };
        let span = anchor.span.round() as i32;
        let start = (anchor.along.round() as i32 - length / 2).clamp(GAP, (span - length - GAP).max(GAP));

        self.window.set_margin(edge, anchor.from_edge.round() as i32 + GAP);
        self.window.set_margin(cross, start);
    }

    fn accept(&self, frame: Frame) {
        let bytes = glib::Bytes::from_owned(frame.pixels);
        let texture = gdk::MemoryTexture::new(
            frame.width as i32,
            frame.height as i32,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            frame.width as usize * 4,
        );
        {
            let mut cache = self.cache.borrow_mut();
            if cache.len() >= CACHE_LIMIT && !cache.contains_key(&frame.address) {
                cache.clear();
            }
            cache.insert(frame.address, texture.clone());
        }
        // A thumbnail for an earlier hover still updates the cache, but only
        // the current strip's tiles are touched.
        if frame.token != self.token.get() {
            return;
        }
        if let Some((picture, icon)) = self.pictures.borrow().get(&frame.address) {
            picture.set_paintable(Some(&texture));
            picture.set_visible(true);
            icon.set_visible(false);
        }
    }

    pub fn close(&self) {
        self.window.close();
    }
}

/// The screen edge the strip shares with the dock, and the edge its position
/// along the dock is measured from.
fn edges(position: Position) -> (Edge, Edge) {
    match position {
        Position::Bottom => (Edge::Bottom, Edge::Left),
        Position::Top => (Edge::Top, Edge::Left),
        Position::Left => (Edge::Left, Edge::Top),
        Position::Right => (Edge::Right, Edge::Top),
    }
}
