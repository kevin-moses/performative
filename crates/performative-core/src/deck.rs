// performative-core/src/deck.rs
//
// Deck and jog state types used throughout the engine and TUI.
//
// Key types:
//   Deck        — per-deck playback, EQ/gain, jog, and duration state
//   JogState    — jog wheel interaction state machine data
//   DeckState   — Empty | Loaded | Playing | Paused
//   JogPhase    — Idle | Scratching | Releasing | Bending
//
// track_duration is populated by engine.rs after a successful b_allocRead + b_query
// roundtrip and is used by the TUI tick loop to stop arc rotation when a song ends.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum DeckState {
    #[default]
    Empty,
    Loaded,
    Playing,
    Paused,
}

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
    /// Total duration of the loaded track in seconds. Populated after a successful
    /// b_allocRead + b_query roundtrip. None until a track is loaded.
    pub track_duration: Option<f32>,
}

impl Deck {
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
        }
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

    /// Reset all playback state (called on load of a new track).
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
    }
}

impl Default for Deck {
    fn default() -> Self {
        Self::new(0)
    }
}
