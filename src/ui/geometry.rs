//! Dock geometry, derived from config and item count.
//!
//! All four edge positions are handled by one set of rules:
//!   * items run along the dock's long axis,
//!   * magnified icons scale about the edge the dock is attached to, so they
//!     grow *away* from the screen edge and never off it,
//!   * the surface carries extra "headroom" on the inward side for them to
//!     grow into, which stays transparent and is not part of the glass panel.

use crate::config::{Config, Position};

#[derive(Debug, Clone)]
pub struct Geometry {
    pub window_w: f64,
    pub window_h: f64,
    /// The glass panel, inset within the surface by the headroom.
    pub panel_x: f64,
    pub panel_y: f64,
    pub panel_w: f64,
    pub panel_h: f64,
    /// Top-left of each item slot, in surface coordinates.
    pub slots: Vec<(f64, f64)>,
    /// Scale origin within an item box, i.e. the point that stays put.
    pub anchor: (f64, f64),
    /// Unit vector pointing away from the screen edge.
    pub lift_dir: (f64, f64),
}

impl Geometry {
    pub fn compute(cfg: &Config, count: usize) -> Self {
        let n = count.max(1) as f64;
        let icon = cfg.dock.icon_size;
        let spacing = cfg.spacing();
        let (px, py) = (cfg.dock.padding_x, cfg.dock.padding_y);
        let head = cfg.headroom();

        // Extent along the dock's long axis, and across it.
        let run = n * icon + (n - 1.0) * spacing;

        if cfg.dock.position.is_vertical() {
            let panel_w = icon + px * 2.0;
            let panel_h = run + py * 2.0;
            // A left-edge dock grows rightwards, so its headroom is on the
            // right and the panel sits flush at x = 0. Mirrored for the right.
            let panel_x = if cfg.dock.position == Position::Left { 0.0 } else { head };
            let slots = (0..count)
                .map(|i| (panel_x + px, py + i as f64 * (icon + spacing)))
                .collect();

            Self {
                window_w: panel_w + head,
                window_h: panel_h,
                panel_x,
                panel_y: 0.0,
                panel_w,
                panel_h,
                slots,
                anchor: if cfg.dock.position == Position::Left {
                    (0.0, icon / 2.0)
                } else {
                    (icon, icon / 2.0)
                },
                lift_dir: if cfg.dock.position == Position::Left { (1.0, 0.0) } else { (-1.0, 0.0) },
            }
        } else {
            let panel_w = run + px * 2.0;
            let panel_h = icon + py * 2.0;
            // A bottom dock grows upwards: headroom above, panel flush below.
            let panel_y = if cfg.dock.position == Position::Bottom { head } else { 0.0 };
            let slots = (0..count)
                .map(|i| (px + i as f64 * (icon + spacing), panel_y + py))
                .collect();

            Self {
                window_w: panel_w,
                window_h: panel_h + head,
                panel_x: 0.0,
                panel_y,
                panel_w,
                panel_h,
                slots,
                anchor: if cfg.dock.position == Position::Bottom {
                    (icon / 2.0, icon)
                } else {
                    (icon / 2.0, 0.0)
                },
                lift_dir: if cfg.dock.position == Position::Bottom { (0.0, -1.0) } else { (0.0, 1.0) },
            }
        }
    }

    /// Which slot contains a surface-local point, ignoring any magnification.
    ///
    /// Hit-testing the *static* slot rather than the scaled icon is deliberate:
    /// a growing icon that also grows its own hit area captures the pointer and
    /// oscillates (grow -> pointer inside -> stay grown), which reads as jitter.
    pub fn slot_at(&self, x: f64, y: f64, icon: f64) -> Option<usize> {
        // Only the panel band counts. A magnified icon extends into the
        // headroom, but the pointer that summoned it is still over the panel.
        if x < self.panel_x
            || x > self.panel_x + self.panel_w
            || y < self.panel_y
            || y > self.panel_y + self.panel_h
        {
            return None;
        }
        self.slots
            .iter()
            .position(|(sx, sy)| x >= *sx && x <= sx + icon && y >= *sy && y <= sy + icon)
    }
}
