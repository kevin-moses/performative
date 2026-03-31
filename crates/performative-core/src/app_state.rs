// performative-core/src/app_state.rs
//
// Application state shared between the TUI render loop and the audio engine.
// `AppState` is wrapped in `Arc<Mutex<AppState>>` and cloned for each render frame.
//
// T5.1 additions:
//   head_deck   — which deck is the BPM reference (0-indexed)
//   cue_mix_up  — whether the cue_mix synth node is alive in scsynth
//   cue_deck    — which deck (if any) is currently routed to the cue (headphone) bus
//   cue_blend   — dry/wet blend for the cue mix output (0.0 = cue only, 1.0 = main)

use std::time::Instant;

use crate::deck::Deck;

/// Tracks an in-progress mix transition so the TUI can display a countdown.
///
/// Created when the engine begins executing a step that contains ramp commands;
/// cleared when all ramps in that step complete.
#[derive(Debug, Clone)]
pub struct ActiveTransition {
    /// Human-readable label for the transition (the original command string).
    pub label: String,
    /// Wall-clock time when this step started.
    pub start: Instant,
    /// Total expected duration of the longest ramp in the step (seconds).
    pub total_secs: f32,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub decks: [Deck; 2],
    pub input_line: String,
    pub status_msg: String,
    /// Guards against creating master_mix node more than once.
    pub master_up: bool,
    pub scsynth_ready: bool,
    /// BPM used for bar/beat -> seconds conversion. Default 120 until Ticket 5.
    pub bpm: f32,
    /// Active mix transition, if any. Drives the countdown display in the status bar.
    pub active_transition: Option<ActiveTransition>,
    /// Which deck (0 or 1) currently has jog focus; None when no deck is engaged.
    pub jog_deck: Option<usize>,
    // ── T5.1 additions ────────────────────────────────────────────────────────
    /// The reference deck for BPM/scheduling (0-indexed). Set by the `head` command.
    /// Defaults to deck 0.
    pub head_deck: usize,
    /// True when the cue_mix synth node exists in scsynth.
    pub cue_mix_up: bool,
    /// Which deck is currently routed to the cue (headphone) bus. None = no cue routing.
    pub cue_deck: Option<usize>,
    /// Cue mix dry/wet blend. 0.0 = cue signal only, 1.0 = main mix only.
    pub cue_blend: f32,
}

impl AppState {
    /// Create a new `AppState` with all fields at their defaults.
    pub fn new() -> Self {
        Self {
            decks: [Deck::new(0), Deck::new(1)],
            input_line: String::new(),
            status_msg: "Booting scsynth…".into(),
            master_up: false,
            scsynth_ready: false,
            bpm: 120.0,
            active_transition: None,
            jog_deck: None,
            head_deck: 0,
            cue_mix_up: false,
            cue_deck: None,
            cue_blend: 0.0,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
