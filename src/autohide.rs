//! Auto-hide policy.
//!
//! Deciding *whether* to hide is separated from the animation that performs it,
//! because the decision is pure arithmetic over rectangles and is the part
//! worth testing. The animation lives in `ui::dock`.
//!
//! Hiding slides the layer surface off-screen with a negative margin, leaving a
//! few pixels on screen as a trigger strip. That sliver keeps receiving pointer
//! events, so no second "trigger" surface is needed — and because the surface
//! genuinely moves off-screen, it stops swallowing clicks meant for the window
//! underneath.

use crate::config::{Config, HideMode, Position};
use crate::hypr::model::{Client, Monitor};

/// An axis-aligned rectangle in Hyprland's layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn overlaps(&self, other: &Rect) -> bool {
        // Touching edges do not count: a maximised window whose edge exactly
        // meets the dock is not actually covered by it.
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

/// Where the dock's visible panel sits on a given monitor, in layout
/// coordinates.
///
/// Takes the *panel* size, not the surface size: the surface also carries
/// transparent magnification headroom and the edge offset, none of which a
/// window can meaningfully overlap.
pub fn dock_rect(cfg: &Config, monitor: &Monitor, panel_w: f64, panel_h: f64) -> Rect {
    let (mx, my) = (monitor.x as f64, monitor.y as f64);
    // Hyprland reports pixel dimensions; layout coordinates are logical.
    let mw = monitor.width as f64 / monitor.scale as f64;
    let mh = monitor.height as f64 / monitor.scale as f64;
    let off = cfg.dock.edge_offset as f64;

    match cfg.dock.position {
        Position::Bottom => Rect {
            x: mx + (mw - panel_w) / 2.0,
            y: my + mh - off - panel_h,
            w: panel_w,
            h: panel_h,
        },
        Position::Top => {
            Rect { x: mx + (mw - panel_w) / 2.0, y: my + off, w: panel_w, h: panel_h }
        }
        Position::Left => {
            Rect { x: mx + off, y: my + (mh - panel_h) / 2.0, w: panel_w, h: panel_h }
        }
        Position::Right => Rect {
            x: mx + mw - off - panel_w,
            y: my + (mh - panel_h) / 2.0,
            w: panel_w,
            h: panel_h,
        },
    }
}

/// Whether the dock should currently be hidden.
///
/// `Intelligent` hides only when a window would actually sit under the dock,
/// which is the behaviour that makes a dock feel unobtrusive without making it
/// disappear on an empty desktop.
pub fn should_hide(
    cfg: &Config,
    dock: &Rect,
    monitor_id: i32,
    clients: &[Client],
    focused: Option<&Client>,
) -> bool {
    match cfg.autohide.mode {
        HideMode::Never => false,
        HideMode::Always => true,
        HideMode::Intelligent => {
            // Only the focused window matters: a background window under the
            // dock is not something the user is looking at.
            let Some(c) = focused else { return false };
            if c.monitor != monitor_id || c.is_special() || !c.mapped || c.hidden {
                return false;
            }
            let _ = clients;
            let win = Rect {
                x: c.at.0 as f64,
                y: c.at.1 as f64,
                w: c.size.0 as f64,
                h: c.size.1 as f64,
            };
            win.overlaps(dock)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypr::Address;

    fn cfg(mode: HideMode) -> Config {
        let mut c = Config::default();
        c.autohide.mode = mode;
        c
    }

    fn client(at: (i32, i32), size: (i32, i32)) -> Client {
        Client {
            address: Address::parse("1"),
            class: "x".into(),
            title: "x".into(),
            initial_class: "x".into(),
            workspace: crate::hypr::model::WorkspaceRef { id: 1, name: "1".into() },
            monitor: 0,
            pid: 1,
            floating: false,
            hidden: false,
            mapped: true,
            fullscreen: 0,
            at,
            size,
            focus_history_id: 0,
        }
    }

    const DOCK: Rect = Rect { x: 400.0, y: 1000.0, w: 500.0, h: 70.0 };

    #[test]
    fn intelligent_hides_only_when_the_focused_window_overlaps() {
        let c = cfg(HideMode::Intelligent);
        // A window filling the screen covers the dock.
        let big = client((0, 0), (1728, 1152));
        assert!(should_hide(&c, &DOCK, 0, &[], Some(&big)));

        // A window stopping above the dock does not.
        let small = client((0, 0), (1728, 900));
        assert!(!should_hide(&c, &DOCK, 0, &[], Some(&small)));
    }

    #[test]
    fn touching_edges_do_not_count_as_overlap() {
        let c = cfg(HideMode::Intelligent);
        // Bottom edge exactly meets the dock's top edge.
        let flush = client((0, 0), (1728, 1000));
        assert!(!should_hide(&c, &DOCK, 0, &[], Some(&flush)));
    }

    #[test]
    fn windows_on_other_monitors_and_scratchpads_are_ignored() {
        let c = cfg(HideMode::Intelligent);
        let mut other = client((0, 0), (1728, 1152));
        other.monitor = 1;
        assert!(!should_hide(&c, &DOCK, 0, &[], Some(&other)));

        let mut scratch = client((0, 0), (1728, 1152));
        scratch.workspace.name = "special:scratchpad".into();
        assert!(!should_hide(&c, &DOCK, 0, &[], Some(&scratch)));
    }

    #[test]
    fn empty_desktop_keeps_the_dock_visible() {
        let c = cfg(HideMode::Intelligent);
        assert!(!should_hide(&c, &DOCK, 0, &[], None));
    }

    #[test]
    fn the_other_modes_ignore_geometry_entirely() {
        let big = client((0, 0), (1728, 1152));
        assert!(!should_hide(&cfg(HideMode::Never), &DOCK, 0, &[], Some(&big)));
        assert!(should_hide(&cfg(HideMode::Always), &DOCK, 0, &[], None));
    }
}
