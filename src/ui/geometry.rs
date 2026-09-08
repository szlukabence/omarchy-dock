//! Dock geometry, derived from config and item count.
//!
//! All four edge positions are handled by one set of rules:
//!   * items run along the dock's long axis,
//!   * magnified icons scale about the edge the dock is attached to, so they
//!     grow *away* from the screen edge and never off it,
//!   * the surface carries extra "headroom" on the inward side for them to
//!     grow into, which stays transparent and is not part of the glass panel.

use crate::config::{Config, Position};
use crate::state::ItemKind;

/// Thickness of a divider slot along the dock's long axis.
const SEPARATOR_EXTENT: f64 = 13.0;

/// A workspace tile is a number, not an icon, so it takes a fraction of an
/// icon's width — the same narrow pill the bar's workspace widget draws.
const WORKSPACE_EXTENT_RATIO: f64 = 0.5;

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
    /// Extent of each slot along the dock's long axis. Separators are narrow,
    /// so slots are no longer uniform and every hit-test and indicator
    /// position has to consult this rather than assume `icon_size`.
    pub extents: Vec<f64>,
    /// Scale origin within an item box, i.e. the point that stays put.
    pub anchor: (f64, f64),
    /// Unit vector pointing away from the screen edge.
    pub lift_dir: (f64, f64),
}

impl Geometry {
    pub fn compute(cfg: &Config, kinds: &[ItemKind]) -> Self {
        let icon = cfg.dock.icon_size;
        let spacing = cfg.spacing();
        let (px, py) = (cfg.dock.padding_x, cfg.dock.padding_y);
        let head = cfg.headroom();

        let extents: Vec<f64> = kinds
            .iter()
            .map(|k| match k {
                ItemKind::Separator => SEPARATOR_EXTENT,
                ItemKind::Workspace => (icon * WORKSPACE_EXTENT_RATIO).round(),
                _ => icon,
            })
            .collect();

        // Total extent along the long axis, plus the gaps between slots.
        let count = extents.len().max(1);
        let run: f64 = extents.iter().sum::<f64>()
            + (count.saturating_sub(1)) as f64 * spacing;
        let run = if extents.is_empty() { icon } else { run };

        // The edge offset is transparent padding *inside* the surface rather
        // than a layer-shell margin, so the surface always reaches the screen
        // edge. Otherwise the gap under a revealed dock is outside it, the
        // pointer at the very edge counts as "left", and hover-reveal
        // oscillates: revealing moves the surface away from the cursor that
        // triggered it.
        let edge = cfg.dock.edge_offset.max(0) as f64;

        if cfg.dock.position.is_vertical() {
            let panel_w = icon + px * 2.0;
            let panel_h = run + py * 2.0;
            // A left-edge dock grows rightwards, so its headroom is on the
            // right and the panel sits flush at x = 0. Mirrored for the right.
            let panel_x =
                if cfg.dock.position == Position::Left { edge } else { head };
            let mut slots = Vec::with_capacity(extents.len());
            let mut cursor = py;
            for e in &extents {
                // Centre narrow slots on the icon column.
                slots.push((panel_x + px, cursor));
                cursor += e + spacing;
            }

            Self {
                window_w: panel_w + head + edge,
                window_h: panel_h,
                panel_x,
                panel_y: 0.0,
                panel_w,
                panel_h,
                slots,
                extents: extents.clone(),
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
            let panel_y =
                if cfg.dock.position == Position::Bottom { head } else { edge };
            let mut slots = Vec::with_capacity(extents.len());
            let mut cursor = px;
            for e in &extents {
                slots.push((cursor, panel_y + py));
                cursor += e + spacing;
            }

            Self {
                window_w: panel_w,
                window_h: panel_h + head + edge,
                panel_x: 0.0,
                panel_y,
                panel_w,
                panel_h,
                slots,
                extents: extents.clone(),
                anchor: if cfg.dock.position == Position::Bottom {
                    (icon / 2.0, icon)
                } else {
                    (icon / 2.0, 0.0)
                },
                lift_dir: if cfg.dock.position == Position::Bottom { (0.0, -1.0) } else { (0.0, 1.0) },
            }
        }
    }

    /// Where an item's running-indicator sits: centred on the slot's cross
    /// axis and tucked against the screen-edge side of the panel, so it stays
    /// put while the icon above it scales.
    pub fn indicator_at(&self, i: usize, _icon: f64, len: f64, thick: f64) -> Option<(f64, f64)> {
        let (sx, sy) = *self.slots.get(i)?;
        let icon = *self.extents.get(i)?;
        // `lift_dir` points away from the screen edge, so negating it walks
        // back towards the edge the dock is anchored to.
        Some(match self.lift_dir {
            (0.0, -1.0) => (sx + (icon - len) / 2.0, self.panel_y + self.panel_h - thick - 3.0),
            (0.0, _) => (sx + (icon - len) / 2.0, self.panel_y + 3.0),
            (-1.0, 0.0) => (self.panel_x + self.panel_w - thick - 3.0, sy + (icon - len) / 2.0),
            _ => (self.panel_x + 3.0, sy + (icon - len) / 2.0),
        })
    }

    /// True when the dock runs horizontally, so indicators are wide and short.
    pub fn horizontal(&self) -> bool {
        self.lift_dir.0 == 0.0
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
        let cross = icon;
        self.slots.iter().enumerate().position(|(i, (sx, sy))| {
            let e = self.extents.get(i).copied().unwrap_or(icon);
            // Long axis uses the slot's own extent; the cross axis is always
            // one icon deep.
            let (w, h) = if self.horizontal() { (e, cross) } else { (cross, e) };
            x >= *sx && x <= sx + w && y >= *sy && y <= sy + h
        })
    }
}
