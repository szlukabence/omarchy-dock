//! Animation primitives.

use crate::config::Config;

/// Damped harmonic oscillator, integrated semi-implicitly.
///
/// Preferred over fixed-duration easing because it is interruptible: when the
/// pointer leaves early the icon reverses from wherever it currently is,
/// carrying its velocity, instead of snapping or restarting a tween.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub pos: f64,
    pub vel: f64,
    pub target: f64,
}

/// Longest frame the integrator will accept. A compositor stall or a laptop
/// resume would otherwise produce a huge `dt` and blow the spring up.
const MAX_DT: f64 = 1.0 / 30.0;

impl Spring {
    pub fn at(v: f64) -> Self {
        Self { pos: v, vel: 0.0, target: v }
    }

    pub fn step(&mut self, dt: f64, stiffness: f64, damping: f64) {
        let dt = dt.clamp(0.0, MAX_DT);
        let accel = stiffness * (self.target - self.pos) - damping * self.vel;
        self.vel += accel * dt;
        self.pos += self.vel * dt;
    }

    pub fn settled(&self) -> bool {
        (self.target - self.pos).abs() < 0.0005 && self.vel.abs() < 0.0005
    }

    /// Snap exactly onto the target so a settled item renders identically each
    /// frame and the tick loop can safely stop.
    pub fn settle(&mut self) {
        self.pos = self.target;
        self.vel = 0.0;
    }

    pub fn step_cfg(&mut self, dt: f64, cfg: &Config) {
        self.step(dt, cfg.magnify.stiffness, cfg.damping());
    }
}
