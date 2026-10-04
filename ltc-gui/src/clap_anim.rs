//! GUI-local clap animation (Phase 5, animation ownership).
//!
//! The engine no longer decays the clap flash/arm on its 25 fps tick; it
//! publishes a monotonic `clap_seq` in `ClapperSnapshot` instead. This
//! module owns the animation on the GUI side: the decay semantics are
//! relocated verbatim from the former `gui_engine::engine::update_clapper_animation`
//! (linear 2.0/s flash decay, exponential 4.0/s arm settle, 1° settle
//! epsilon), but sampled at the GUI's own frame rate instead of the
//! engine's 40 ms tick.

use std::time::{Duration, Instant};

/// Arm rest angle in radians (relocated verbatim from engine.rs).
pub const TARGET_ARM_ANGLE: f32 = -25.0 * std::f32::consts::PI / 180.0;
/// Settle epsilon in radians — the arm is "at rest" within 1 degree (relocated verbatim).
pub const ARM_SETTLE_EPS: f32 = 1.0 * std::f32::consts::PI / 180.0; // 1 degree

const FLASH_DECAY_PER_SEC: f32 = 2.0;
const ARM_DECAY_PER_SEC: f32 = 4.0;

/// Fullscreen white-flash opacity at `elapsed_secs` after the clap
/// (1.0 on clap, linear decay to 0 at 2.0/s).
pub fn flash_alpha_at(elapsed_secs: f32) -> f32 {
    (1.0 - elapsed_secs * FLASH_DECAY_PER_SEC).max(0.0)
}

/// Clapper arm angle in radians at `elapsed_secs` after the clap
/// (exponential settle toward [`TARGET_ARM_ANGLE`] at 4.0/s from the
/// struck 0.0 angle).
pub fn arm_angle_at(elapsed_secs: f32) -> f32 {
    TARGET_ARM_ANGLE + (0.0 - TARGET_ARM_ANGLE) * (-ARM_DECAY_PER_SEC * elapsed_secs).exp()
}

/// Settle condition, relocated verbatim from the engine's `animating` flag:
/// flash fully decayed AND the arm within [`ARM_SETTLE_EPS`] of rest.
pub fn animation_settled(elapsed_secs: f32) -> bool {
    flash_alpha_at(elapsed_secs) <= 0.0
        && (arm_angle_at(elapsed_secs) - TARGET_ARM_ANGLE).abs() <= ARM_SETTLE_EPS
}

/// A clap animation in flight, keyed by the engine's `clap_seq`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClapAnim {
    pub started: Instant,
    pub seq: u64,
}

impl ClapAnim {
    pub fn new(seq: u64, now: Instant) -> Self {
        ClapAnim { started: now, seq }
    }

    pub fn elapsed(&self, now: Instant) -> f32 {
        now.duration_since(self.started).as_secs_f32()
    }

    pub fn is_settled(&self, now: Instant) -> bool {
        animation_settled(self.elapsed(now))
    }
}

/// egui 0.35 (ContextImpl::request_repaint_after, context.rs:148-151) subtracts
/// `predicted_dt` (fixed at 1/60 s — eframe/egui-winit never override it) from
/// every requested delay to avoid over-shooting.  We must compensate by adding
/// the same duration back, and floor the result to ensure a call with `≤ predicted_dt`
/// after compensation never becomes zero (which would cause an unbounded render
/// storm — see profiling analysis for context).
///
/// The animator's repaint request goes through the exact same compensation as
/// `next_repaint_interval`; it must never bypass the floor.
pub fn animation_repaint_delay(anim: &ClapAnim, now: Instant) -> Option<Duration> {
    if anim.is_settled(now) {
        return None;
    }
    let predicted_dt = Duration::from_secs_f64(1.0 / 60.0);
    let floor = predicted_dt + Duration::from_millis(1);
    Some((predicted_dt + predicted_dt).max(floor))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Decay semantics (relocated verbatim from engine.rs) ──────────────

    #[test]
    fn flash_at_zero_is_full() {
        assert!((flash_alpha_at(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn flash_decays_linearly_and_clamps_at_zero() {
        let a = flash_alpha_at(0.04);
        assert!(a > 0.0 && a < 1.0, "small elapsed must not clamp");
        assert!((a - (1.0 - 0.04 * 2.0)).abs() < 1e-5, "linear 2.0/s decay");
        assert_eq!(flash_alpha_at(10.0), 0.0, "large elapsed must clamp at zero");
    }

    #[test]
    fn arm_at_zero_is_struck() {
        assert!(arm_angle_at(0.0).abs() < 1e-6, "arm starts at the struck 0° angle");
    }

    #[test]
    fn arm_settles_toward_rest_within_epsilon() {
        assert!(arm_angle_at(10.0) < 0.0, "rest angle is negative (raised arm)");
        assert!(
            (arm_angle_at(10.0) - TARGET_ARM_ANGLE).abs() < ARM_SETTLE_EPS,
            "arm must settle within epsilon of the rest angle",
        );
    }

    #[test]
    fn arm_decay_matches_engine_exponential_curve() {
        // One engine tick's worth (40 ms) must match the engine's
        // `angle += (target - angle) * (1 - exp(-4 * dt))` step from 0.
        let dt = 0.04;
        let engine_step = 0.0f32 + (TARGET_ARM_ANGLE - 0.0) * (1.0 - (-4.0f32 * dt).exp());
        assert!((arm_angle_at(dt) - engine_step).abs() < 1e-5);
    }

    #[test]
    fn settled_false_while_flash_visible() {
        assert!(!animation_settled(0.0), "fresh clap must not be settled");
        assert!(!animation_settled(0.04));
    }

    #[test]
    fn settled_true_after_decay_completes() {
        assert!(animation_settled(10.0), "fully decayed clap must be settled");
    }

    // ── ClapAnim state machine ───────────────────────────────────────────

    #[test]
    fn clap_anim_tracks_seq_and_settles() {
        let start = Instant::now();
        let anim = ClapAnim::new(7, start);
        assert_eq!(anim.seq, 7);
        assert!(!anim.is_settled(start), "fresh animator must be running");
        // Settle time: flash 0 at 0.5 s; arm needs ln(25)/4 ≈ 0.805 s.
        let later = start + Duration::from_millis(900);
        assert!(anim.is_settled(later), "animator must stop after the settle window");
    }

    #[test]
    fn clap_anim_elapsed_measures_from_start() {
        let start = Instant::now();
        let anim = ClapAnim::new(1, start);
        let at = start + Duration::from_millis(100);
        assert!((anim.elapsed(at) - 0.1).abs() < 1e-3);
    }

    // ── Repaint delay (render-storm guard) ───────────────────────────────

    #[test]
    fn animation_repaint_delay_none_once_settled() {
        let start = Instant::now();
        let anim = ClapAnim::new(1, start);
        let later = start + Duration::from_secs(10);
        assert!(animation_repaint_delay(&anim, later).is_none(),
            "a settled animator must not request further repaints");
    }

    #[test]
    fn animation_repaint_delay_running_applies_predicted_dt_compensation() {
        let start = Instant::now();
        let anim = ClapAnim::new(1, start);
        let delay = animation_repaint_delay(&anim, start)
            .expect("running animator must request repaints");
        // base 1/60 s + predicted_dt compensation = 33.33 ms (the old
        // `clapper.animating` repaint branch), floored at 1/60 + 1 ms.
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        assert!(delay >= floor);
        assert!(delay <= Duration::from_millis(35), "animation repaints at ~60 fps, got {:?}", delay);
    }
}
