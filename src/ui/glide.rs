//! Where icons start when the dock's set of items changes, so that they glide
//! to their new places instead of jumping there.
//!
//! A swap builds new icons in new slots. Each icon that was already in the
//! dock starts drawn where it was on screen and at the size it was drawn at,
//! and springs from there to its slot; an icon that is new grows in from
//! nothing. The glass panel eases from its old rectangle to its new one.
//!
//! The dock's surface is centred along its long axis, so a surface-local
//! position means a different place on screen once the surface's length
//! changes. Every start here is worked out so that the screen position stays
//! the same at the moment of the swap. Across the axis nothing moves: the
//! icons and the panel keep their distance from the screen edge whatever the
//! icon size, by how the geometry is laid out.
//!
//! Pure arithmetic over plain numbers: no GTK, so all of it is tested here.

/// A rectangle in terms of the dock's axes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Where it starts along the dock's long axis.
    pub along: f64,
    /// Where it starts across it.
    pub across: f64,
    pub length: f64,
    pub depth: f64,
}

impl Rect {
    /// The rectangle a fraction `t` of the way from `self` to `to`.
    pub fn lerp(&self, to: &Rect, t: f64) -> Rect {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        Rect {
            along: mix(self.along, to.along),
            across: mix(self.across, to.across),
            length: mix(self.length, to.length),
            depth: mix(self.depth, to.depth),
        }
    }
}

/// The dock as drawn at the moment of a swap, in its surface's coordinates.
#[derive(Debug, Clone)]
pub struct Before {
    /// Each item's match key, in dock order.
    pub keys: Vec<String>,
    /// Each item's centre along the long axis, as drawn.
    pub centres: Vec<f64>,
    /// Each item's drawn size: its icon size times any scale it is drawn at.
    pub sizes: Vec<f64>,
    /// The surface's length along the long axis, and its depth across it.
    pub length: f64,
    pub depth: f64,
    /// The panel, as drawn.
    pub panel: Rect,
}

/// The new layout, before any room is added for a glide.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Each slot's centre along the long axis.
    pub centres: Vec<f64>,
    /// The icon size the new slots are laid out for.
    pub icon: f64,
    pub length: f64,
    pub depth: f64,
    /// Whether the screen edge the dock sits on is at the far end of the
    /// cross axis: the bottom or the right of the surface.
    pub far_edge: bool,
}

/// Where everything starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Glide {
    /// Room added at each end of the new surface. A shrinking dock keeps its
    /// old length until the glide is over, or the icons and panel ends that
    /// start outside the new surface would be cut off.
    pub pad: f64,
    /// For each new slot: how far along the axis its icon starts from the
    /// slot, and the scale it starts at. `None` for a new item, which grows in
    /// from nothing.
    pub starts: Vec<Option<(f64, f64)>>,
    /// The panel's starting rectangle in the new, padded surface.
    pub panel: Rect,
}

/// Work out where everything starts when `before` turns into `after`, whose
/// items have the match keys `keys`.
pub fn plan(before: &Before, keys: &[&str], after: &Layout) -> Glide {
    let pad = ((before.length - after.length) / 2.0).max(0.0);
    // What to add to a position in the old surface to name the same place on
    // screen in the new one: both are centred, so half the change in length.
    let delta = (after.length + 2.0 * pad - before.length) / 2.0;

    // Each old item answers for one new one at most, in order, so dividers
    // that share a key pair up rather than all claiming the first.
    let mut taken = vec![false; before.keys.len()];
    let starts = keys
        .iter()
        .enumerate()
        .map(|(j, key)| {
            let k = (0..before.keys.len()).find(|&k| !taken[k] && before.keys[k] == *key)?;
            taken[k] = true;
            let slot = after.centres.get(j)? + pad;
            let shift = before.centres[k] + delta - slot;
            Some((shift, before.sizes[k] / after.icon))
        })
        .collect();

    // Across the axis, the panel keeps its distance from the screen edge.
    let across = if after.far_edge {
        after.depth - (before.depth - before.panel.across)
    } else {
        before.panel.across
    };
    let panel = Rect { along: before.panel.along + delta, across, ..before.panel };

    Glide { pad, starts, panel }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen this wide, for the tests to compare screen positions on.
    const SCREEN: f64 = 2000.0;

    /// Where a surface-local position along the axis is on screen.
    fn on_screen(local: f64, length: f64) -> f64 {
        (SCREEN - length) / 2.0 + local
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// `n` items of `icon` with `gap` between them, starting at `pad_x`.
    fn row(keys: &[&str], icon: f64, gap: f64, pad_x: f64) -> (Vec<f64>, f64) {
        let centres = (0..keys.len()).map(|i| pad_x + i as f64 * (icon + gap) + icon / 2.0).collect();
        let length = 2.0 * pad_x + keys.len() as f64 * icon + (keys.len() as f64 - 1.0) * gap;
        (centres, length)
    }

    fn before(keys: &[&str], icon: f64) -> Before {
        let (centres, length) = row(keys, icon, 4.0, 10.0);
        Before {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            centres,
            sizes: vec![icon; keys.len()],
            length,
            depth: icon + 50.0,
            panel: Rect { along: 0.0, across: 30.0, length, depth: icon + 20.0 },
        }
    }

    fn layout(keys: &[&str], icon: f64) -> Layout {
        let (centres, length) = row(keys, icon, 4.0, 10.0);
        Layout {
            centres,
            icon,
            length,
            depth: icon + 50.0,
            far_edge: true,
        }
    }

    /// Every item that stays starts exactly where it was on screen.
    fn assert_nothing_jumps(b: &Before, keys: &[&str], a: &Layout, g: &Glide) {
        let length = a.length + 2.0 * g.pad;
        let mut taken = vec![false; b.keys.len()];
        for (j, start) in g.starts.iter().enumerate() {
            let Some((shift, _)) = start else { continue };
            let k = (0..b.keys.len()).find(|&k| !taken[k] && b.keys[k] == keys[j]).unwrap();
            taken[k] = true;
            let was = on_screen(b.centres[k], b.length);
            let starts = on_screen(a.centres[j] + g.pad + shift, length);
            assert!(close(was, starts), "{}: was at {was}, starts at {starts}", keys[j]);
        }
    }

    #[test]
    fn icons_make_room_for_a_new_one_from_where_they_were() {
        let b = before(&["a", "b", "c"], 60.0);
        let keys = ["a", "new", "b", "c"];
        let a = layout(&keys, 60.0);
        let g = plan(&b, &keys, &a);
        assert_eq!(g.pad, 0.0, "a growing dock needs no extra room");
        assert_eq!(g.starts.len(), 4);
        assert_eq!(g.starts[1], None, "the new icon grows in");
        assert_nothing_jumps(&b, &keys, &a, &g);
        // Its neighbours move apart: the first one left, the others right.
        assert!(g.starts[0].unwrap().0 > 0.0);
        assert!(g.starts[2].unwrap().0 < 0.0);
        assert!(g.starts[3].unwrap().0 < 0.0);
    }

    #[test]
    fn a_shrinking_dock_keeps_its_length_until_the_glide_is_over() {
        let b = before(&["a", "gone", "b", "c"], 60.0);
        let keys = ["a", "b", "c"];
        let a = layout(&keys, 60.0);
        let g = plan(&b, &keys, &a);
        assert!(close(a.length + 2.0 * g.pad, b.length), "the surface keeps its old length");
        assert_nothing_jumps(&b, &keys, &a, &g);
        // The panel starts as long as it was, where it was on screen.
        assert!(close(g.panel.length, b.panel.length));
        assert!(close(
            on_screen(g.panel.along, a.length + 2.0 * g.pad),
            on_screen(b.panel.along, b.length)
        ));
    }

    #[test]
    fn icons_shrinking_to_fit_start_at_the_size_they_were() {
        let b = before(&["a", "b"], 60.0);
        let keys = ["a", "b", "new"];
        let a = layout(&keys, 55.0);
        let g = plan(&b, &keys, &a);
        let (_, scale) = g.starts[0].unwrap();
        assert!(close(scale * 55.0, 60.0), "drawn at {}", scale * 55.0);
        assert_nothing_jumps(&b, &keys, &a, &g);
    }

    #[test]
    fn dividers_sharing_a_key_pair_up_in_order() {
        let b = before(&["a", "|", "b", "|", "c"], 60.0);
        let keys = ["a", "|", "b", "new", "|", "c"];
        let a = layout(&keys, 60.0);
        let g = plan(&b, &keys, &a);
        assert_eq!(g.starts[3], None);
        assert!(g.starts.iter().enumerate().all(|(j, s)| j == 3 || s.is_some()));
        assert_nothing_jumps(&b, &keys, &a, &g);
    }

    #[test]
    fn the_panel_keeps_its_distance_from_the_screen_edge() {
        // A bottom dock whose icons shrink gets a shallower surface; the
        // panel's bottom stays where it was on screen.
        let b = before(&["a"], 60.0);
        let keys = ["a", "new"];
        let a = layout(&keys, 55.0);
        let g = plan(&b, &keys, &a);
        assert!(close(g.panel.depth, b.panel.depth), "it starts as deep as it was");
        let gap_before = b.depth - (b.panel.across + b.panel.depth);
        let gap_after = a.depth - (g.panel.across + g.panel.depth);
        assert!(close(gap_before, gap_after));

        // A top dock measures from the top instead.
        let mut b = b;
        b.panel.across = 25.0;
        let top = Layout { far_edge: false, ..layout(&keys, 55.0) };
        assert!(close(plan(&b, &keys, &top).panel.across, 25.0));
    }

    #[test]
    fn the_panel_eases_between_two_rectangles() {
        let from = Rect { along: 0.0, across: 10.0, length: 100.0, depth: 40.0 };
        let to = Rect { along: 20.0, across: 14.0, length: 60.0, depth: 36.0 };
        assert_eq!(from.lerp(&to, 0.0), from);
        assert_eq!(from.lerp(&to, 1.0), to);
        assert_eq!(from.lerp(&to, 0.5), Rect { along: 10.0, across: 12.0, length: 80.0, depth: 38.0 });
    }
}
