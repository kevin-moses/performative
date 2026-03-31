// performative-core/src/deck.rs
//
// Deck and jog state types used throughout the engine and TUI.
//
// Key types:
//   Deck        — per-deck playback, EQ/gain, jog, loop, cue, and buffer state
//   BufferInfo  — metadata returned by scsynth /b_info (frames, channels, sample rate)
//   LoopState   — active loop region (in/out points and length in bars)
//   JogState    — jog wheel interaction state machine data
//   DeckState   — Empty | Loaded | Playing | Paused
//   JogPhase    — Idle | Scratching | Releasing | Bending
//
// buffer_info is populated by engine.rs after a successful b_allocRead + b_query
// roundtrip.  track_duration is derived from buffer_info for backwards compatibility.
// cue_points persist per-track across sessions (not cleared on reset_playback).

use std::collections::HashMap;
use std::time::{Duration, Instant};

// ── Buffer metadata ───────────────────────────────────────────────────────────

/// Metadata for a scsynth buffer, returned by the /b_info OSC reply.
///
/// Populated after a successful b_allocRead + b_query roundtrip in engine.rs.
#[derive(Debug, Clone, Default)]
pub struct BufferInfo {
    pub num_frames: i32,
    pub num_channels: i32,
    pub sample_rate: f32,
}

impl BufferInfo {
    /// Duration of the buffer in seconds.
    ///
    /// purpose: compute track length from frame count and sample rate.
    /// @return: duration in seconds, or 0.0 if sample_rate is zero
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate > 0.0 {
            self.num_frames as f32 / self.sample_rate
        } else {
            0.0
        }
    }

    /// Convert a position in seconds to a frame index.
    ///
    /// purpose: translate a seek target from seconds to the integer frame offset
    ///          required by scsynth's PlayBuf pos argument.
    /// @param secs: (f32) position in seconds
    /// @return: frame index (truncated, not rounded)
    pub fn secs_to_frames(&self, secs: f32) -> i32 {
        (secs * self.sample_rate) as i32
    }
}

// ── Loop state ────────────────────────────────────────────────────────────────

/// The active loop region for a deck.
///
/// Created when the user sets a loop; cleared with `loop off`.
/// The loop monitor in engine.rs polls playhead_secs_f32() and seeks back to
/// in_secs when the playhead crosses out_secs.
#[derive(Debug, Clone)]
pub struct LoopState {
    /// Loop start point in seconds.
    pub in_secs: f32,
    /// Loop end point in seconds.
    pub out_secs: f32,
    /// Original length of the loop in bars (used for halve/double operations).
    pub length_bars: f32,
}

// ── DeckState ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub enum DeckState {
    #[default]
    Empty,
    Loaded,
    Playing,
    Paused,
}

// ── RampParam ─────────────────────────────────────────────────────────────────

/// Which gain parameter a pending ramp targets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RampParam { Gain, Lo, Mid, Hi }

/// A ramp command queued while the deck was not playing.
/// Applied (sent to scsynth) as soon as the deck resumes.
#[derive(Debug, Clone)]
pub struct PendingRamp {
    pub param: RampParam,
    pub target: f32,
    pub duration_secs: f32,
}

// ── JogMode / JogPhase ────────────────────────────────────────────────────────

/// Whether the jog wheel operates in vinyl-scratch mode or pitch-bend mode.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum JogMode {
    #[default]
    Vinyl,
    PitchBend,
}

/// Current phase of the jog wheel interaction state machine.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum JogPhase {
    /// No jog activity; wheel is at rest.
    #[default]
    Idle,
    /// Finger is pressing the platter; playback rate is driven by wheel velocity.
    Scratching,
    /// Finger has lifted; playback is returning to base rate.
    Releasing,
    /// Wheel is nudging the rate up or down without full scratch engagement.
    Bending,
}

// ── JogState ──────────────────────────────────────────────────────────────────

/// All mutable state for one deck's jog wheel, updated by the MIDI handler.
#[derive(Debug, Clone)]
pub struct JogState {
    /// Current phase of the jog state machine.
    pub phase: JogPhase,
    /// Vinyl-scratch or pitch-bend mode.
    pub mode: JogMode,
    /// Smoothed velocity (radians/sec, positive = forward).
    pub velocity: f32,
    /// Wall-clock time of the most recent jog event.
    pub last_event: Instant,
    /// Wall-clock time when the current phase began.
    pub phase_start: Instant,
    /// Playback rate before jog engagement (restored on release).
    pub base_rate: f32,
    /// Rate currently being sent to scsynth (base_rate modified by jog).
    pub effective_rate: f32,
    /// Accumulated angular position of the platter (radians).
    pub arc_position: f32,
    /// Kalman/low-pass filter position accumulator.
    pub filter_pos: f32,
    /// Kalman/low-pass filter velocity accumulator.
    pub filter_vel: f32,
}

impl JogState {
    /// Create a new `JogState` with all fields at their default (idle) values.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            phase: JogPhase::Idle,
            mode: JogMode::Vinyl,
            velocity: 0.0,
            last_event: now,
            phase_start: now,
            base_rate: 1.0,
            effective_rate: 1.0,
            arc_position: 0.0,
            filter_pos: 0.0,
            filter_vel: 0.0,
        }
    }
}

impl Default for JogState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Deck ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Deck {
    pub index: usize,
    pub state: DeckState,
    pub track_name: Option<String>,
    pub track_path: Option<String>,
    /// Whether player+eq synths have been created in scsynth for this deck.
    pub synths_up: bool,
    /// Accumulated play time before the current segment (i.e. at last pause).
    pub elapsed_before: Duration,
    /// When the current play segment started (None if not playing).
    pub play_start: Option<Instant>,
    /// Live parameter state (mirrors what's running in scsynth, used for UI).
    pub gain: f32,
    pub lo_gain: f32,
    pub mid_gain: f32,
    pub hi_gain: f32,
    /// Ramps queued while the deck was paused/stopped; drained on play().
    pub pending_ramps: Vec<PendingRamp>,
    /// Jog wheel interaction state.
    pub jog: JogState,
    /// Accumulated playback time in seconds, adjusted for rate changes.
    /// Updated by the TUI tick loop. More accurate than wall-clock for jog scenarios.
    pub playback_elapsed: f32,
    /// Total duration of the loaded track in seconds. Derived from buffer_info
    /// for backwards compatibility. None until a track is loaded.
    pub track_duration: Option<f32>,
    // ── T5.1 additions ────────────────────────────────────────────────────────
    /// Full buffer metadata returned by scsynth /b_info after load.
    pub buffer_info: Option<BufferInfo>,
    /// BPM detected by the analysis crate (None until analysis completes).
    pub native_bpm: Option<f32>,
    /// Musical key detected by the analysis crate (e.g. "A minor"). None until analysis completes.
    pub key: Option<String>,
    /// Effective playback BPM when synced; mirrors rate * native_bpm.
    pub playing_bpm: Option<f32>,
    /// Current playback rate multiplier sent to scsynth. Default 1.0.
    pub rate: f32,
    /// Named cue points in seconds. Keyed by a single character label (e.g. 'A').
    /// Persists across loads — not cleared by reset_playback().
    pub cue_points: HashMap<char, f32>,
    /// Active loop region, if any. Cleared by reset_playback().
    pub loop_state: Option<LoopState>,
    /// True when this deck is routed to the cue (headphone) bus.
    pub cue_active: bool,
    /// True when this deck's rate is locked to the head deck's BPM.
    pub synced: bool,
}

impl Deck {
    /// Create a new `Deck` with all fields at their defaults.
    ///
    /// purpose: initialise a clean, empty deck slot.
    /// @param index: (usize) 0-based deck index
    /// @return: fully initialised Deck
    pub fn new(index: usize) -> Self {
        Self {
            index,
            state: DeckState::Empty,
            track_name: None,
            track_path: None,
            synths_up: false,
            elapsed_before: Duration::ZERO,
            play_start: None,
            gain: 1.0,
            lo_gain: 1.0,
            mid_gain: 1.0,
            hi_gain: 1.0,
            pending_ramps: Vec::new(),
            jog: JogState::new(),
            playback_elapsed: 0.0,
            track_duration: None,
            buffer_info: None,
            native_bpm: None,
            key: None,
            playing_bpm: None,
            rate: 1.0,
            cue_points: HashMap::new(),
            loop_state: None,
            cue_active: false,
            synced: false,
        }
    }

    /// Current playhead position in seconds (rate-adjusted), as a float.
    ///
    /// purpose: return the precise playhead position for seek/loop/cue operations.
    ///          playback_elapsed tracks wall-time accumulation; the rate multiplier
    ///          converts that to actual buffer position.
    /// @return: playhead position in seconds as f32
    pub fn playhead_secs_f32(&self) -> f32 {
        self.playback_elapsed.max(0.0) * self.rate
    }

    /// Total elapsed playhead time in seconds, rate-adjusted.
    ///
    /// purpose: return the display-facing playhead position, driven by
    ///          `playback_elapsed` which is updated by the TUI tick loop
    ///          at the current effective playback rate.
    /// @return: playhead position in whole seconds
    pub fn playhead_secs(&self) -> u64 {
        self.playback_elapsed.max(0.0) as u64
    }

    /// Advance the rate-adjusted playhead by one tick.
    ///
    /// purpose: increment `playback_elapsed` by `dt * rate`, clamped at 0.0.
    ///          Called from the TUI tick loop (~30 fps) while the deck is playing.
    /// @param dt: (f32) tick interval in seconds (typically 0.033)
    /// @param rate: (f32) current effective playback rate (jog rate or 1.0 at normal speed)
    pub fn advance_playhead(&mut self, dt: f32, rate: f32) {
        self.playback_elapsed = (self.playback_elapsed + dt * rate).max(0.0);
    }

    /// Format playhead as "M:SS".
    pub fn playhead_display(&self) -> String {
        let secs = self.playhead_secs();
        format!("{}:{:02}", secs / 60, secs % 60)
    }

    /// Reset all playback state when a new track is loaded.
    ///
    /// purpose: tear down synth/playhead/loop/sync state for a new load.
    ///          cue_points are intentionally preserved so hot cues survive
    ///          across loads within the same session.
    pub fn reset_playback(&mut self) {
        self.synths_up = false;
        self.elapsed_before = Duration::ZERO;
        self.play_start = None;
        self.state = DeckState::Loaded;
        self.gain = 1.0;
        self.lo_gain = 1.0;
        self.mid_gain = 1.0;
        self.hi_gain = 1.0;
        self.pending_ramps.clear();
        self.jog = JogState::new();
        self.playback_elapsed = 0.0;
        self.track_duration = None;
        // T5.1 fields reset on load:
        self.buffer_info = None;
        self.rate = 1.0;
        self.loop_state = None;
        self.synced = false;
        self.playing_bpm = None;
        self.cue_active = false;
        // cue_points intentionally NOT cleared — they persist per-track.
    }
}

impl Default for Deck {
    fn default() -> Self {
        Self::new(0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BufferInfo::duration_secs ─────────────────────────────────────────────

    #[test]
    fn buffer_info_duration_secs_exact_one_second() {
        let info = BufferInfo { num_frames: 44100, num_channels: 2, sample_rate: 44100.0 };
        let dur = info.duration_secs();
        assert!((dur - 1.0).abs() < 1e-6, "expected 1.0, got {dur}");
    }

    #[test]
    fn buffer_info_duration_secs_zero_sample_rate_returns_zero() {
        let info = BufferInfo { num_frames: 44100, num_channels: 2, sample_rate: 0.0 };
        assert_eq!(info.duration_secs(), 0.0);
    }

    #[test]
    fn buffer_info_duration_secs_fractional() {
        // 22050 frames at 44100 Hz = 0.5 seconds
        let info = BufferInfo { num_frames: 22050, num_channels: 2, sample_rate: 44100.0 };
        let dur = info.duration_secs();
        assert!((dur - 0.5).abs() < 1e-6, "expected 0.5, got {dur}");
    }

    // ── BufferInfo::secs_to_frames ────────────────────────────────────────────

    #[test]
    fn buffer_info_secs_to_frames_roundtrip() {
        let info = BufferInfo { num_frames: 441000, num_channels: 2, sample_rate: 44100.0 };
        let frames = info.secs_to_frames(1.0);
        assert_eq!(frames, 44100);
    }

    #[test]
    fn buffer_info_secs_to_frames_truncates_not_rounds() {
        let info = BufferInfo { num_frames: 441000, num_channels: 2, sample_rate: 44100.0 };
        // 0.9999... seconds * 44100 should truncate, not round up
        let frames = info.secs_to_frames(0.5);
        assert_eq!(frames, 22050);
    }

    #[test]
    fn buffer_info_secs_to_frames_zero() {
        let info = BufferInfo { num_frames: 441000, num_channels: 2, sample_rate: 44100.0 };
        assert_eq!(info.secs_to_frames(0.0), 0);
    }

    // ── Deck::playhead_secs_f32 ───────────────────────────────────────────────

    #[test]
    fn playhead_secs_f32_at_normal_rate() {
        let mut deck = Deck::new(0);
        deck.playback_elapsed = 30.0;
        deck.rate = 1.0;
        let pos = deck.playhead_secs_f32();
        assert!((pos - 30.0).abs() < 1e-6, "expected 30.0, got {pos}");
    }

    #[test]
    fn playhead_secs_f32_accounts_for_rate_above_one() {
        let mut deck = Deck::new(0);
        deck.playback_elapsed = 10.0;
        deck.rate = 1.5;
        // Buffer advances at 1.5x: 10.0 * 1.5 = 15.0
        let pos = deck.playhead_secs_f32();
        assert!((pos - 15.0).abs() < 1e-6, "expected 15.0, got {pos}");
    }

    #[test]
    fn playhead_secs_f32_accounts_for_rate_below_one() {
        let mut deck = Deck::new(0);
        deck.playback_elapsed = 20.0;
        deck.rate = 0.5;
        // Buffer advances at half speed: 20.0 * 0.5 = 10.0
        let pos = deck.playhead_secs_f32();
        assert!((pos - 10.0).abs() < 1e-6, "expected 10.0, got {pos}");
    }

    #[test]
    fn playhead_secs_f32_clamps_negative_elapsed_to_zero() {
        let mut deck = Deck::new(0);
        deck.playback_elapsed = -5.0;
        deck.rate = 1.0;
        let pos = deck.playhead_secs_f32();
        assert_eq!(pos, 0.0);
    }

    // ── Deck::reset_playback ──────────────────────────────────────────────────

    #[test]
    fn reset_playback_clears_loop_state() {
        let mut deck = Deck::new(0);
        deck.loop_state = Some(LoopState { in_secs: 4.0, out_secs: 8.0, length_bars: 4.0 });
        deck.reset_playback();
        assert!(deck.loop_state.is_none());
    }

    #[test]
    fn reset_playback_resets_rate_to_one() {
        let mut deck = Deck::new(0);
        deck.rate = 1.25;
        deck.reset_playback();
        assert!((deck.rate - 1.0).abs() < 1e-6);
    }

    #[test]
    fn reset_playback_clears_synced() {
        let mut deck = Deck::new(0);
        deck.synced = true;
        deck.reset_playback();
        assert!(!deck.synced);
    }

    #[test]
    fn reset_playback_clears_playing_bpm() {
        let mut deck = Deck::new(0);
        deck.playing_bpm = Some(128.0);
        deck.reset_playback();
        assert!(deck.playing_bpm.is_none());
    }

    #[test]
    fn reset_playback_clears_cue_active() {
        let mut deck = Deck::new(0);
        deck.cue_active = true;
        deck.reset_playback();
        assert!(!deck.cue_active);
    }

    #[test]
    fn reset_playback_preserves_cue_points() {
        let mut deck = Deck::new(0);
        deck.cue_points.insert('A', 32.5);
        deck.cue_points.insert('B', 96.0);
        deck.reset_playback();
        // Cue points must survive a load/reset cycle.
        assert_eq!(deck.cue_points.get(&'A'), Some(&32.5));
        assert_eq!(deck.cue_points.get(&'B'), Some(&96.0));
    }

    #[test]
    fn reset_playback_clears_buffer_info() {
        let mut deck = Deck::new(0);
        deck.buffer_info = Some(BufferInfo { num_frames: 44100, num_channels: 2, sample_rate: 44100.0 });
        deck.reset_playback();
        assert!(deck.buffer_info.is_none());
    }

    #[test]
    fn reset_playback_sets_state_to_loaded() {
        let mut deck = Deck::new(0);
        deck.state = DeckState::Playing;
        deck.reset_playback();
        assert_eq!(deck.state, DeckState::Loaded);
    }

    // ── Default / new ─────────────────────────────────────────────────────────

    #[test]
    fn deck_new_has_rate_one() {
        let deck = Deck::new(0);
        assert!((deck.rate - 1.0).abs() < 1e-6);
    }

    #[test]
    fn deck_new_has_empty_cue_points() {
        let deck = Deck::new(0);
        assert!(deck.cue_points.is_empty());
    }

    #[test]
    fn deck_new_has_no_loop_state() {
        let deck = Deck::new(0);
        assert!(deck.loop_state.is_none());
    }

    #[test]
    fn deck_new_not_synced() {
        let deck = Deck::new(0);
        assert!(!deck.synced);
    }

    #[test]
    fn deck_new_cue_active_false() {
        let deck = Deck::new(0);
        assert!(!deck.cue_active);
    }
}
