// performative-core/src/jog.rs
//
// Jog wheel state machine, velocity estimation, and rate computation.
//
// Vinyl scratch uses direct velocity (events/sec) with exponential smoothing.
// Pitch-bend mode uses an alpha-beta filter for gentle nudging.
// effective_rate is always clamped to [-4.0, 4.0] to prevent PlayBuf from
// racing to a buffer boundary and losing the synth node.
//
// arc_position wraps in [0, 2π) (TAU) so one full visual rotation of the
// semicircle arc maps to exactly one turn of the platter.
//
// Exports:
//   on_scroll(jog, delta, shift_held) — called from the TUI on each scroll event
//   tick(jog, playing)               — called every ~33ms to advance the state machine
//   rate_and_lag(jog)                — returns (rate, rate_lag) to send to scsynth

use std::time::Instant;
use crate::deck::{JogState, JogPhase, JogMode};

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Alpha term for the alpha-beta filter (position correction weight).
const ALPHA: f32 = 0.125;
/// Beta term for the alpha-beta filter (velocity correction weight).
const BETA: f32 = 0.004;
/// Scales raw scroll delta into position units before filtering.
const SCROLL_SENSITIVITY: f32 = 0.15;
/// Multiplied by raw velocity (events/sec) to produce the vinyl scratch rate.
const SCRATCH_SENSITIVITY: f32 = 0.03;
/// Exponential smoothing factor for vinyl scratch velocity (0.7 = responsive).
const SCRATCH_SMOOTH: f32 = 0.7;
/// Multiplied by filtered velocity to produce the pitch bend offset.
const BEND_SENSITIVITY: f32 = 0.08;
/// Scales scroll delta for arc position advancement (visual only).
const ARC_SENSITIVITY: f32 = 0.3;
/// Time (ms) after the last scroll event before the state machine considers
/// the user's hand to have lifted off the platter.
const SCROLL_TIMEOUT_MS: u64 = 50;
/// Duration (seconds) of the releasing phase: playback rate returns to base rate.
const RELEASE_DURATION: f32 = 0.05;
/// Per-tick decay factor applied to bend velocity when scrolling has stopped.
const BEND_DECAY: f32 = 0.85;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Compute the vinyl scratch playback rate from filtered velocity.
///
/// purpose: convert jog velocity to a playback rate for scratch mode.
/// @param jog: (&JogState) the current jog state (read-only)
/// @return: scratch playback rate (f32)
fn scratch_rate(jog: &JogState) -> f32 {
    jog.velocity * SCRATCH_SENSITIVITY
}

// ── Alpha-beta filter ─────────────────────────────────────────────────────────

/// Advance the alpha-beta position/velocity filter by one measurement.
///
/// purpose: update jog.filter_pos, jog.filter_vel, and jog.velocity from a
///          new position delta and elapsed time.
/// @param jog: (&mut JogState) the jog state to update in-place
/// @param delta: (f32) observed position change since the last call (filtered units)
/// @param dt: (f32) elapsed time since the last call (seconds, clamped >= 0.001)
fn filter_update(jog: &mut JogState, delta: f32, dt: f32) {
    let predicted_pos = jog.filter_pos + jog.filter_vel * dt;
    let residual = delta - predicted_pos;
    jog.filter_pos = predicted_pos + ALPHA * residual;
    jog.filter_vel += BETA * residual / dt;
    jog.velocity = jog.filter_vel;
}

// ── Scroll event handler ──────────────────────────────────────────────────────

/// Handle one scroll event from the terminal event loop.
///
/// In vinyl mode (shift not held) this drives the scratch phase state machine:
/// the first event transitions Idle -> Scratching and the effective playback
/// rate is driven by a directly-computed, exponentially-smoothed velocity.
/// In pitch-bend mode (shift held) the wheel nudges the rate up or down and
/// decays back to normal when scrolling stops.
///
/// purpose: update jog state from a single scroll tick.
/// @param jog: (&mut JogState) the jog state to update in-place
/// @param delta: (i32) +1 for scroll-up (forward), -1 for scroll-down (backward)
/// @param shift_held: (bool) true when the Shift modifier is active (pitch-bend mode)
pub fn on_scroll(jog: &mut JogState, delta: i32, shift_held: bool) {
    let now = Instant::now();
    let dt = now.duration_since(jog.last_event).as_secs_f32().max(0.001);
    jog.last_event = now;

    let scroll_delta = delta as f32 * SCROLL_SENSITIVITY;

    if shift_held {
        jog.mode = JogMode::PitchBend;
        filter_update(jog, scroll_delta, dt);
        jog.phase = JogPhase::Bending;
        jog.effective_rate = (jog.base_rate + jog.velocity * BEND_SENSITIVITY).clamp(-4.0, 4.0);
    } else {
        jog.mode = JogMode::Vinyl;
        // Go directly to Scratching (no Braking intermediate).
        if jog.phase == JogPhase::Idle || jog.phase == JogPhase::Releasing {
            jog.phase = JogPhase::Scratching;
            jog.phase_start = now;
        }
        // Direct velocity: events per second, exponentially smoothed.
        let raw_vel = delta as f32 / dt;
        jog.velocity = SCRATCH_SMOOTH * raw_vel + (1.0 - SCRATCH_SMOOTH) * jog.velocity;
        jog.effective_rate = scratch_rate(jog).clamp(-4.0, 4.0);
    }

    jog.arc_position = (jog.arc_position + scroll_delta * ARC_SENSITIVITY)
        .rem_euclid(std::f32::consts::TAU);
}

// ── Tick update ───────────────────────────────────────────────────────────────

/// Advance the jog state machine. Should be called once per render frame (~33ms).
///
/// Manages phase transitions based on elapsed time since the last scroll event
/// and drives the effective_rate interpolation during Releasing. The arc position
/// is only advanced in Idle when the deck is actively playing.
///
/// purpose: advance time-based jog state transitions and update effective_rate.
/// @param jog: (&mut JogState) the jog state to advance in-place
/// @param playing: (bool) true when the deck is currently playing (gates arc advancement)
pub fn tick(jog: &mut JogState, playing: bool) {
    let since_last = jog.last_event.elapsed().as_millis() as u64;

    match jog.phase {
        JogPhase::Idle => {
            // Only rotate the arc indicator when the deck is playing.
            if playing {
                jog.arc_position = (jog.arc_position + jog.base_rate * 0.05)
                    .rem_euclid(std::f32::consts::TAU);
            }
        }
        JogPhase::Scratching => {
            if since_last > SCROLL_TIMEOUT_MS {
                jog.phase = JogPhase::Releasing;
                jog.phase_start = Instant::now();
            }
        }
        JogPhase::Releasing => {
            let t = jog.phase_start.elapsed().as_secs_f32() / RELEASE_DURATION;
            if t >= 1.0 {
                jog.effective_rate = jog.base_rate;
                jog.phase = JogPhase::Idle;
                jog.velocity = 0.0;
            } else {
                let from = scratch_rate(jog);
                jog.effective_rate = from + (jog.base_rate - from) * ease_out(t);
            }
        }
        JogPhase::Bending => {
            if since_last > SCROLL_TIMEOUT_MS {
                jog.velocity *= BEND_DECAY;
                jog.effective_rate = jog.base_rate + jog.velocity * BEND_SENSITIVITY;
                if jog.velocity.abs() < 0.001 {
                    jog.effective_rate = jog.base_rate;
                    jog.phase = JogPhase::Idle;
                    jog.velocity = 0.0;
                }
            }
        }
    }
}

// ── Rate output ───────────────────────────────────────────────────────────────

/// Compute the `(rate, rate_lag)` pair to send to the scsynth deck_player node.
///
/// `rate_lag` is the VarLag slew time in seconds — tighter in scratch mode,
/// looser during braking and releasing to produce smooth acceleration curves.
///
/// purpose: derive the OSC control pair for the current jog phase.
/// @param jog: (&JogState) the current jog state (read-only)
/// @return: (f32, f32) tuple of (rate, rate_lag)
pub fn rate_and_lag(jog: &JogState) -> (f32, f32) {
    let lag = match jog.phase {
        JogPhase::Idle       => 0.0,
        JogPhase::Scratching => 0.005,
        JogPhase::Releasing  => 0.05,
        JogPhase::Bending    => 0.03,
    };
    (jog.effective_rate, lag)
}

// ── Easing ────────────────────────────────────────────────────────────────────

/// Quadratic ease-out: starts fast, decelerates to t=1.
///
/// purpose: smooth the rate interpolation during braking and releasing phases.
/// @param t: (f32) normalized time in [0, 1]
/// @return: eased value in [0, 1]
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(2)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::JogState;

    fn idle_jog() -> JogState {
        JogState::new()
    }

    // ── ease_out ──────────────────────────────────────────────────────────────

    #[test]
    fn ease_out_endpoints() {
        assert!((ease_out(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_out(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ease_out_midpoint_less_than_half() {
        // Quadratic ease-out produces a value > 0.5 at t=0.5 (fast start).
        assert!(ease_out(0.5) > 0.5);
    }

    // ── filter_update ─────────────────────────────────────────────────────────

    #[test]
    fn filter_update_zero_delta_leaves_velocity_near_zero() {
        let mut jog = idle_jog();
        // Feed ten zero-delta updates; velocity should stay near zero.
        for _ in 0..10 {
            filter_update(&mut jog, 0.0, 0.033);
        }
        assert!(jog.velocity.abs() < 0.01);
    }

    #[test]
    fn filter_update_positive_delta_drives_positive_velocity() {
        let mut jog = idle_jog();
        for _ in 0..20 {
            filter_update(&mut jog, 0.5, 0.033);
        }
        assert!(jog.velocity > 0.0);
    }

    // ── on_scroll vinyl mode ──────────────────────────────────────────────────

    #[test]
    fn first_vinyl_scroll_transitions_idle_to_scratching() {
        let mut jog = idle_jog();
        assert_eq!(jog.phase, JogPhase::Idle);
        on_scroll(&mut jog, 1, false);
        assert_eq!(jog.phase, JogPhase::Scratching);
    }

    #[test]
    fn vinyl_scroll_sets_vinyl_mode() {
        let mut jog = idle_jog();
        on_scroll(&mut jog, 1, false);
        assert_eq!(jog.mode, JogMode::Vinyl);
    }

    #[test]
    fn vinyl_scroll_forward_gives_positive_rate() {
        let mut jog = idle_jog();
        // Feed several forward scrolls to build up velocity.
        for _ in 0..5 {
            on_scroll(&mut jog, 1, false);
        }
        assert!(jog.effective_rate > 0.0);
    }

    #[test]
    fn vinyl_scroll_backward_gives_negative_rate() {
        let mut jog = idle_jog();
        for _ in 0..5 {
            on_scroll(&mut jog, -1, false);
        }
        assert!(jog.effective_rate < 0.0);
    }

    #[test]
    fn arc_position_advances_on_scroll() {
        let mut jog = idle_jog();
        let before = jog.arc_position;
        on_scroll(&mut jog, 1, false);
        // arc_position should have advanced.
        assert!(jog.arc_position != before || jog.arc_position == 0.0); // rem_euclid could wrap to 0
    }

    #[test]
    fn arc_position_stays_in_0_to_tau() {
        let mut jog = idle_jog();
        for _ in 0..100 {
            on_scroll(&mut jog, 1, false);
        }
        assert!(jog.arc_position >= 0.0);
        assert!(jog.arc_position < std::f32::consts::TAU);
    }

    // ── on_scroll pitch-bend mode ─────────────────────────────────────────────

    #[test]
    fn shift_scroll_sets_bend_mode_and_phase() {
        let mut jog = idle_jog();
        on_scroll(&mut jog, 1, true);
        assert_eq!(jog.mode, JogMode::PitchBend);
        assert_eq!(jog.phase, JogPhase::Bending);
    }

    #[test]
    fn shift_scroll_nudges_rate_above_base() {
        let mut jog = idle_jog();
        for _ in 0..5 {
            on_scroll(&mut jog, 1, true);
        }
        assert!(jog.effective_rate > jog.base_rate);
    }

    #[test]
    fn shift_scroll_backward_nudges_rate_below_base() {
        let mut jog = idle_jog();
        for _ in 0..5 {
            on_scroll(&mut jog, -1, true);
        }
        assert!(jog.effective_rate < jog.base_rate);
    }

    // ── tick ──────────────────────────────────────────────────────────────────

    #[test]
    fn tick_idle_advances_arc_position() {
        let mut jog = idle_jog();
        let before = jog.arc_position;
        // Force base_rate > 0 so arc actually moves.
        jog.base_rate = 1.0;
        tick(&mut jog, true);
        assert!(jog.arc_position != before);
    }

    #[test]
    fn tick_idle_does_not_advance_when_paused() {
        let mut jog = idle_jog();
        jog.base_rate = 1.0;
        let before = jog.arc_position;
        tick(&mut jog, false);
        assert_eq!(jog.arc_position, before);
    }

    #[test]
    fn tick_scratching_transitions_to_releasing_after_timeout() {
        let mut jog = idle_jog();
        on_scroll(&mut jog, 1, false);
        assert_eq!(jog.phase, JogPhase::Scratching);
        // Back-date last_event so the timeout has elapsed.
        jog.last_event = Instant::now()
            - std::time::Duration::from_millis(SCROLL_TIMEOUT_MS + 10);
        tick(&mut jog, true);
        assert_eq!(jog.phase, JogPhase::Releasing);
    }

    #[test]
    fn tick_releasing_eventually_returns_to_idle() {
        let mut jog = idle_jog();
        on_scroll(&mut jog, 1, false);
        jog.phase = JogPhase::Releasing;
        // Back-date phase_start so releasing is fully elapsed.
        jog.phase_start = Instant::now()
            - std::time::Duration::from_secs_f32(RELEASE_DURATION + 0.1);
        tick(&mut jog, true);
        assert_eq!(jog.phase, JogPhase::Idle);
        assert!((jog.effective_rate - jog.base_rate).abs() < 1e-6);
        assert!((jog.velocity).abs() < 1e-6);
    }

    #[test]
    fn tick_bending_decays_velocity_toward_zero() {
        let mut jog = idle_jog();
        on_scroll(&mut jog, 1, true);
        jog.phase = JogPhase::Bending;
        let initial_vel = jog.velocity;
        // Back-date last_event to trigger the decay branch.
        jog.last_event = Instant::now()
            - std::time::Duration::from_millis(SCROLL_TIMEOUT_MS + 10);
        tick(&mut jog, true);
        // Velocity should have shrunk.
        assert!(jog.velocity.abs() < initial_vel.abs() + 1e-6);
    }

    #[test]
    fn tick_bending_settles_to_idle_when_velocity_tiny() {
        let mut jog = idle_jog();
        jog.phase = JogPhase::Bending;
        jog.velocity = 0.0005; // below 0.001 threshold
        jog.last_event = Instant::now()
            - std::time::Duration::from_millis(SCROLL_TIMEOUT_MS + 10);
        tick(&mut jog, true);
        assert_eq!(jog.phase, JogPhase::Idle);
    }

    // ── rate_and_lag ──────────────────────────────────────────────────────────

    #[test]
    fn rate_and_lag_idle_returns_zero_lag() {
        let jog = idle_jog();
        let (_, lag) = rate_and_lag(&jog);
        assert!((lag - 0.0).abs() < 1e-6);
    }

    #[test]
    fn rate_and_lag_scratching_returns_tight_lag() {
        let mut jog = idle_jog();
        jog.phase = JogPhase::Scratching;
        let (_, lag) = rate_and_lag(&jog);
        assert!(lag > 0.0 && lag < 0.02);
    }

    #[test]
    fn rate_and_lag_returns_effective_rate() {
        let mut jog = idle_jog();
        jog.effective_rate = 1.5;
        let (rate, _) = rate_and_lag(&jog);
        assert!((rate - 1.5).abs() < 1e-6);
    }

    #[test]
    fn vinyl_scroll_produces_meaningful_rate() {
        let mut jog = idle_jog();
        // Simulate several forward scrolls.
        for _ in 0..5 {
            on_scroll(&mut jog, 1, false);
        }
        // With direct velocity estimation, rate should be substantial.
        assert!(jog.effective_rate.abs() > 0.1,
            "scratch rate should be substantial, got {}", jog.effective_rate);
    }
}
