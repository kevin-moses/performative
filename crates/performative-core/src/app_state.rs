// performative-core/src/app_state.rs
//
// Application state shared between the TUI render loop and the audio engine.
// `AppState` is wrapped in `Arc<Mutex<AppState>>` and cloned for each render frame.

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
}

impl AppState {
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
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
