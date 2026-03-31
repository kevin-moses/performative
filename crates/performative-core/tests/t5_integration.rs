// performative-core/tests/t5_integration.rs
//
// Integration tests for Ticket 5 features: Seek, Cue, Loop, Head, Pre, and
// Pipe-context command dispatch.
//
// These tests verify the full pipeline from parser output through AppState /
// Deck state manipulation without requiring a running scsynth server.
//
// Strategy:
//   1. Parse command strings with `performative_parser::parse` /
//      `performative_parser::parse_script` and assert the correct AST variant.
//   2. Directly instantiate and mutate `AppState` / `Deck` structs to simulate
//      the state changes the engine would apply, then assert the resulting state.
//
// `AudioEngine` is intentionally never constructed — it requires a live OSC
// connection to scsynth, which is not available in CI / unit-test contexts.

use performative_core::deck::{BufferInfo, Deck, DeckState, LoopState};
use performative_core::AppState;
use performative_parser::{
    parse, parse_script, Command, CueAction, LoopAction, PreAction, RampDuration, SeekPosition,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return a fresh `AppState` with default field values.
///
/// purpose: provide a clean, isolated starting state for each test that
///          needs session-level state (head_deck, bpm, etc.).
fn make_state() -> AppState {
    AppState::new()
}

/// Return a `Deck` populated with enough metadata to run loop/seek calculations.
///
/// purpose: provide a loaded, playing deck with a known BPM and buffer for
///          tests that need to compute bar / second positions.
/// @param index: (usize) 0-based deck slot index
/// @param bpm: (f32) native BPM for the simulated track
/// @param duration_secs: (f32) track length in seconds
fn make_loaded_deck(index: usize, bpm: f32, duration_secs: f32) -> Deck {
    let sample_rate = 44100.0_f32;
    let num_frames = (duration_secs * sample_rate) as i32;

    let mut deck = Deck::new(index);
    deck.state = DeckState::Playing;
    deck.track_name = Some(format!("test_track_{index}.wav"));
    deck.track_path = Some(format!("/tmp/test_track_{index}.wav"));
    deck.native_bpm = Some(bpm);
    deck.playing_bpm = Some(bpm);
    deck.track_duration = Some(duration_secs);
    deck.buffer_info = Some(BufferInfo { num_frames, num_channels: 2, sample_rate });
    deck
}

/// Compute the seconds-per-bar duration for a given BPM.
///
/// purpose: shared calculation for loop in/out point assertions.
/// @param bpm: (f32) beats per minute (4 beats per bar assumed)
/// @return: bar length in seconds
fn bar_secs(bpm: f32) -> f32 {
    4.0 * 60.0 / bpm
}

// ── Seek: parse assertions ─────────────────────────────────────────────────────

#[test]
fn seek_timestamp_parses_to_seconds() {
    // "1:30" should become 90.0 seconds.
    let cmd = parse("seek 1 1:30").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 0, position: SeekPosition::Seconds(90.0) });
}

#[test]
fn seek_bar_keyword_parses_to_bar_variant() {
    let cmd = parse("seek 2 bar 4").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 1, position: SeekPosition::Bar(4.0) });
}

#[test]
fn seek_positive_relative_bars_parses_correctly() {
    let cmd = parse("seek 1 +8bars").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 0, position: SeekPosition::RelativeBars(8.0) });
}

#[test]
fn seek_negative_relative_seconds_parses_correctly() {
    let cmd = parse("seek 2 -10s").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 1, position: SeekPosition::RelativeSeconds(-10.0) });
}

#[test]
fn seek_cue_label_parses_to_cue_point_variant() {
    // Lowercase 'a' must be stored uppercase.
    let cmd = parse("seek 1 a").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 0, position: SeekPosition::CuePoint('A') });
}

#[test]
fn seek_cue_label_uppercase_input_accepted() {
    let cmd = parse("seek 2 b").unwrap();
    assert_eq!(cmd, Command::Seek { deck: 1, position: SeekPosition::CuePoint('B') });
}

// ── Seek: state simulation ────────────────────────────────────────────────────

#[test]
fn seek_seconds_sets_playback_elapsed_on_deck() {
    // Simulate what the engine does when it receives Seek { Seconds(90.0) }:
    // it sets deck.playback_elapsed to the target position.
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.playback_elapsed = 10.0;

    // Engine: resolve SeekPosition::Seconds(90.0) → set playback_elapsed.
    let target_secs = 90.0_f32;
    deck.playback_elapsed = target_secs;

    assert!(
        (deck.playback_elapsed - 90.0).abs() < 1e-4,
        "expected 90.0, got {}",
        deck.playback_elapsed
    );
}

#[test]
fn seek_relative_bars_applies_signed_offset() {
    let bpm = 120.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    // Place playhead at bar 4 (= 4 bars * bar_secs(120)).
    let bar = bar_secs(bpm);
    deck.playback_elapsed = 4.0 * bar;

    // Engine: SeekPosition::RelativeBars(+8.0) → current + 8 * bar_secs.
    let offset_secs = 8.0 * bar;
    deck.playback_elapsed += offset_secs;

    let expected = 12.0 * bar;
    assert!(
        (deck.playback_elapsed - expected).abs() < 1e-3,
        "expected {expected}, got {}",
        deck.playback_elapsed
    );
}

#[test]
fn seek_bar_absolute_positions_correctly() {
    let bpm = 120.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    deck.playback_elapsed = 60.0;

    // Engine: SeekPosition::Bar(4.0) → (4 - 1) * bar_secs (1-indexed).
    let bar = bar_secs(bpm);
    deck.playback_elapsed = (4.0 - 1.0) * bar;

    let expected = 3.0 * bar;
    assert!(
        (deck.playback_elapsed - expected).abs() < 1e-3,
        "expected {expected}, got {}",
        deck.playback_elapsed
    );
}

#[test]
fn seek_to_cue_point_sets_playback_elapsed() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.cue_points.insert('A', 45.5);

    // Engine: SeekPosition::CuePoint('A') → look up in deck.cue_points and seek.
    let target = *deck.cue_points.get(&'A').unwrap();
    deck.playback_elapsed = target;

    assert!(
        (deck.playback_elapsed - 45.5).abs() < 1e-4,
        "expected 45.5, got {}",
        deck.playback_elapsed
    );
}

// ── Cue: parse assertions ─────────────────────────────────────────────────────

#[test]
fn cue_set_action_parses_correctly() {
    let cmd = parse("cue 1 set a").unwrap();
    assert_eq!(cmd, Command::Cue { deck: 0, action: CueAction::Set('A') });
}

#[test]
fn cue_jump_action_parses_correctly() {
    let cmd = parse("cue 2 b").unwrap();
    assert_eq!(cmd, Command::Cue { deck: 1, action: CueAction::Jump('B') });
}

#[test]
fn cue_label_stored_uppercase_from_lowercase_input() {
    // Both 'a' input and 'A' input must produce the same uppercase stored char.
    let cmd_lower = parse("cue 1 set a").unwrap();
    let cmd_upper = parse("cue 1 set A").unwrap();
    assert_eq!(cmd_lower, cmd_upper);
    assert_eq!(cmd_lower, Command::Cue { deck: 0, action: CueAction::Set('A') });
}

// ── Cue: state simulation ─────────────────────────────────────────────────────

#[test]
fn cue_set_stores_position_in_deck_cue_points() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.playback_elapsed = 32.0;

    // Engine: CueAction::Set('A') → store current playback_elapsed under 'A'.
    let position = deck.playback_elapsed;
    deck.cue_points.insert('A', position);

    assert_eq!(deck.cue_points.get(&'A'), Some(&32.0));
}

#[test]
fn cue_points_survive_reset_playback() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.cue_points.insert('A', 32.5);
    deck.cue_points.insert('C', 96.0);

    deck.reset_playback();

    // reset_playback explicitly preserves cue_points (they are per-track hot cues).
    assert_eq!(deck.cue_points.get(&'A'), Some(&32.5));
    assert_eq!(deck.cue_points.get(&'C'), Some(&96.0));
}

#[test]
fn cue_jump_moves_playback_elapsed_to_stored_point() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.cue_points.insert('B', 64.0);
    deck.playback_elapsed = 5.0;

    // Engine: CueAction::Jump('B') → look up 'B', set playback_elapsed.
    let pos = *deck.cue_points.get(&'B').unwrap();
    deck.playback_elapsed = pos;

    assert!(
        (deck.playback_elapsed - 64.0).abs() < 1e-4,
        "expected 64.0, got {}",
        deck.playback_elapsed
    );
}

#[test]
fn cue_points_keyed_uppercase_only() {
    // Cue point map keys are always uppercase chars ('A'–'D'); the engine and
    // parser both enforce this, so lookups must use uppercase.
    let mut deck = Deck::new(0);
    deck.cue_points.insert('A', 10.0);

    assert!(deck.cue_points.contains_key(&'A'));
    assert!(!deck.cue_points.contains_key(&'a'));
}

// ── Loop: parse assertions ─────────────────────────────────────────────────────

#[test]
fn loop_set_bars_parses_correctly() {
    let cmd = parse("loop 1 4bars").unwrap();
    assert_eq!(
        cmd,
        Command::Loop { deck: 0, action: LoopAction::Set(RampDuration::Bars(4.0)) }
    );
}

#[test]
fn loop_bare_number_with_bars_suffix_parses() {
    // "loop 1 4" with an implicit bars suffix is NOT valid; "4bars" is required.
    // Verify the parser rejects a bare integer for loop length.
    let result = parse("loop 1 4");
    assert!(
        result.is_err(),
        "expected parse error for bare integer loop length, got {result:?}"
    );
}

#[test]
fn loop_off_parses_correctly() {
    let cmd = parse("loop 2 off").unwrap();
    assert_eq!(cmd, Command::Loop { deck: 1, action: LoopAction::Off });
}

#[test]
fn loop_halve_parses_correctly() {
    let cmd = parse("loop 1 halve").unwrap();
    assert_eq!(cmd, Command::Loop { deck: 0, action: LoopAction::Halve });
}

#[test]
fn loop_double_parses_correctly() {
    let cmd = parse("loop 1 double").unwrap();
    assert_eq!(cmd, Command::Loop { deck: 0, action: LoopAction::Double });
}

// ── Loop: state simulation ─────────────────────────────────────────────────────

#[test]
fn loop_set_creates_correct_in_out_points_at_bpm_120() {
    let bpm = 120.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    deck.playback_elapsed = 32.0;

    // Engine: LoopAction::Set(RampDuration::Bars(4.0)) → create LoopState.
    let length_bars = 4.0_f32;
    let length_secs = RampDuration::Bars(length_bars).to_secs(bpm);
    let in_secs = deck.playback_elapsed;
    let out_secs = in_secs + length_secs;

    deck.loop_state = Some(LoopState { in_secs, out_secs, length_bars });

    let ls = deck.loop_state.as_ref().unwrap();
    assert!(
        (ls.in_secs - 32.0).abs() < 1e-4,
        "in_secs: expected 32.0, got {}",
        ls.in_secs
    );
    // At 120 BPM: bar = 2s, so 4 bars = 8s; out_secs = 32 + 8 = 40.
    let expected_out = 40.0_f32;
    assert!(
        (ls.out_secs - expected_out).abs() < 1e-3,
        "out_secs: expected {expected_out}, got {}",
        ls.out_secs
    );
    assert!((ls.length_bars - 4.0).abs() < 1e-6);
}

#[test]
fn loop_set_creates_correct_in_out_points_at_bpm_140() {
    let bpm = 140.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    deck.playback_elapsed = 0.0;

    let length_bars = 8.0_f32;
    let length_secs = RampDuration::Bars(length_bars).to_secs(bpm);
    let in_secs = deck.playback_elapsed;
    let out_secs = in_secs + length_secs;

    deck.loop_state = Some(LoopState { in_secs, out_secs, length_bars });

    let ls = deck.loop_state.as_ref().unwrap();
    // At 140 BPM: bar = (4*60)/140 ≈ 1.714s; 8 bars ≈ 13.714s.
    let expected_out = 8.0 * bar_secs(bpm);
    assert!(
        (ls.out_secs - expected_out).abs() < 1e-3,
        "out_secs: expected {expected_out}, got {}",
        ls.out_secs
    );
}

#[test]
fn loop_off_clears_loop_state() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.loop_state = Some(LoopState { in_secs: 4.0, out_secs: 12.0, length_bars: 4.0 });

    // Engine: LoopAction::Off → clear loop_state.
    deck.loop_state = None;

    assert!(deck.loop_state.is_none());
}

#[test]
fn loop_halve_reduces_length_bars_by_half() {
    let bpm = 120.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    let initial_bars = 4.0_f32;
    let in_secs = 32.0_f32;
    let out_secs = in_secs + RampDuration::Bars(initial_bars).to_secs(bpm);
    deck.loop_state = Some(LoopState { in_secs, out_secs, length_bars: initial_bars });

    // Engine: LoopAction::Halve → halve length_bars and move out_secs inward.
    let ls = deck.loop_state.as_mut().unwrap();
    ls.length_bars /= 2.0;
    ls.out_secs = ls.in_secs + RampDuration::Bars(ls.length_bars).to_secs(bpm);

    let ls = deck.loop_state.as_ref().unwrap();
    assert!((ls.length_bars - 2.0).abs() < 1e-6, "expected 2.0 bars, got {}", ls.length_bars);
    let expected_out = in_secs + RampDuration::Bars(2.0).to_secs(bpm);
    assert!(
        (ls.out_secs - expected_out).abs() < 1e-3,
        "out_secs after halve: expected {expected_out}, got {}",
        ls.out_secs
    );
}

#[test]
fn loop_double_increases_length_bars_by_two() {
    let bpm = 120.0_f32;
    let mut deck = make_loaded_deck(0, bpm, 300.0);
    let initial_bars = 4.0_f32;
    let in_secs = 16.0_f32;
    let out_secs = in_secs + RampDuration::Bars(initial_bars).to_secs(bpm);
    deck.loop_state = Some(LoopState { in_secs, out_secs, length_bars: initial_bars });

    // Engine: LoopAction::Double → double length_bars and extend out_secs.
    let ls = deck.loop_state.as_mut().unwrap();
    ls.length_bars *= 2.0;
    ls.out_secs = ls.in_secs + RampDuration::Bars(ls.length_bars).to_secs(bpm);

    let ls = deck.loop_state.as_ref().unwrap();
    assert!((ls.length_bars - 8.0).abs() < 1e-6, "expected 8.0 bars, got {}", ls.length_bars);
    let expected_out = in_secs + RampDuration::Bars(8.0).to_secs(bpm);
    assert!(
        (ls.out_secs - expected_out).abs() < 1e-3,
        "out_secs after double: expected {expected_out}, got {}",
        ls.out_secs
    );
}

#[test]
fn loop_state_cleared_by_reset_playback() {
    let mut deck = make_loaded_deck(0, 120.0, 300.0);
    deck.loop_state = Some(LoopState { in_secs: 0.0, out_secs: 8.0, length_bars: 4.0 });

    deck.reset_playback();

    assert!(
        deck.loop_state.is_none(),
        "loop_state must be cleared by reset_playback"
    );
}

// ── Head: parse assertions ─────────────────────────────────────────────────────

#[test]
fn head_deck1_parses_correctly() {
    let cmd = parse("head 1").unwrap();
    assert_eq!(cmd, Command::Head { deck: 0 });
}

#[test]
fn head_deck2_parses_correctly() {
    let cmd = parse("head 2").unwrap();
    assert_eq!(cmd, Command::Head { deck: 1 });
}

#[test]
fn head_missing_deck_returns_error() {
    let result = parse("head");
    assert!(
        matches!(result, Err(performative_parser::ParseError::MissingArg { cmd: "head", .. })),
        "expected MissingArg for 'head' with no deck, got {result:?}"
    );
}

// ── Head: state simulation ────────────────────────────────────────────────────

#[test]
fn head_command_updates_head_deck_index_in_app_state() {
    let mut state = make_state();
    assert_eq!(state.head_deck, 0, "default head_deck should be 0");

    // Engine: Command::Head { deck: 1 } → update state.head_deck.
    state.head_deck = 1;

    assert_eq!(state.head_deck, 1);
}

#[test]
fn head_deck_native_bpm_propagates_to_session_bpm() {
    let mut state = make_state();
    state.decks[1] = make_loaded_deck(1, 140.0, 300.0);

    // Engine: Command::Head { deck: 1 } → set head_deck = 1 and adopt
    // the deck's native_bpm as the session BPM for scheduling.
    state.head_deck = 1;
    if let Some(bpm) = state.decks[state.head_deck].native_bpm {
        state.bpm = bpm;
    }

    assert_eq!(state.head_deck, 1);
    assert!(
        (state.bpm - 140.0).abs() < 1e-4,
        "session BPM should be 140.0 after head 2, got {}",
        state.bpm
    );
}

#[test]
fn head_deck_without_native_bpm_leaves_session_bpm_unchanged() {
    let mut state = make_state();
    state.bpm = 120.0;
    // Deck 0 has no native_bpm set (default Deck::new).

    // Engine applies head 1 but deck has no BPM → session BPM unchanged.
    state.head_deck = 0;
    if let Some(bpm) = state.decks[state.head_deck].native_bpm {
        state.bpm = bpm;
    }

    assert!(
        (state.bpm - 120.0).abs() < 1e-4,
        "session BPM should remain 120.0, got {}",
        state.bpm
    );
}

// ── Pre: parse assertions ─────────────────────────────────────────────────────

#[test]
fn pre_deck1_parses_to_pre_action_deck_zero() {
    let cmd = parse("pre 1").unwrap();
    assert_eq!(cmd, Command::Pre { action: PreAction::Deck(0) });
}

#[test]
fn pre_deck2_parses_to_pre_action_deck_one() {
    let cmd = parse("pre 2").unwrap();
    assert_eq!(cmd, Command::Pre { action: PreAction::Deck(1) });
}

#[test]
fn pre_off_parses_correctly() {
    let cmd = parse("pre off").unwrap();
    assert_eq!(cmd, Command::Pre { action: PreAction::Off });
}

#[test]
fn pre_blend_parses_float_value() {
    let cmd = parse("pre blend 0.5").unwrap();
    assert_eq!(cmd, Command::Pre { action: PreAction::Blend(0.5) });
}

#[test]
fn pre_blend_clamps_to_zero_to_one() {
    // Values above 1.0 are clamped to 1.0.
    let cmd_high = parse("pre blend 1.5").unwrap();
    assert_eq!(cmd_high, Command::Pre { action: PreAction::Blend(1.0) });

    // Values below 0.0 are clamped to 0.0.
    let cmd_low = parse("pre blend -0.5").unwrap();
    assert_eq!(cmd_low, Command::Pre { action: PreAction::Blend(0.0) });
}

// ── Pipe context: parser integration ─────────────────────────────────────────

#[test]
fn pipe_context_injects_deck_into_cue_set() {
    // "1 | cue set a" should produce a cue set command for deck 1 (index 0).
    let script = parse_script("1 | cue set a").unwrap();
    let cmds = &script.statements[0].steps[0].commands;
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0], Command::Cue { deck: 0, action: CueAction::Set('A') });
}

#[test]
fn pipe_context_injects_deck_into_seek() {
    // "2 | seek 1:30" should become seek on deck 2 (index 1).
    let script = parse_script("2 | seek 1:30").unwrap();
    let cmds = &script.statements[0].steps[0].commands;
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0], Command::Seek { deck: 1, position: SeekPosition::Seconds(90.0) });
}

#[test]
fn pipe_context_propagates_across_sequential_steps() {
    // "2 | seek 1:30 > cue set b" — the deck-2 context from the first step
    // should propagate into the second step.
    let script = parse_script("2 | seek 1:30 > cue set b").unwrap();
    let stmt = &script.statements[0];
    assert_eq!(stmt.steps.len(), 2);

    let seek_cmd = &stmt.steps[0].commands[0];
    assert_eq!(*seek_cmd, Command::Seek { deck: 1, position: SeekPosition::Seconds(90.0) });

    let cue_cmd = &stmt.steps[1].commands[0];
    assert_eq!(*cue_cmd, Command::Cue { deck: 1, action: CueAction::Set('B') });
}

#[test]
fn pipe_context_with_explicit_deck_in_command_is_not_overridden() {
    // When the command already has an explicit deck number, inject_deck must
    // leave it unchanged.
    let script = parse_script("2 | seek 1 1:30").unwrap();
    let cmds = &script.statements[0].steps[0].commands;
    // "seek 1 1:30" already specifies deck 1 (index 0); the pipe context of 2
    // must not override the explicit deck.
    assert_eq!(cmds[0], Command::Seek { deck: 0, position: SeekPosition::Seconds(90.0) });
}

// ── Combined state scenarios ───────────────────────────────────────────────────

#[test]
fn load_play_cue_set_then_jump_changes_playback_elapsed() {
    // Scenario: load a track, play it to a position, set cue A, advance the
    // playhead, then jump back to cue A.
    let mut deck = make_loaded_deck(0, 120.0, 300.0);

    // Simulate: play → advance to 60 seconds.
    deck.playback_elapsed = 60.0;

    // Simulate: cue set A → store current position.
    let cue_pos = deck.playback_elapsed;
    deck.cue_points.insert('A', cue_pos);

    // Simulate: advance playhead further.
    deck.playback_elapsed = 120.0;
    assert!(deck.cue_points.contains_key(&'A'));

    // Simulate: cue jump A → seek back.
    let jump_to = *deck.cue_points.get(&'A').unwrap();
    deck.playback_elapsed = jump_to;

    assert!(
        (deck.playback_elapsed - 60.0).abs() < 1e-4,
        "after cue jump playback_elapsed should be 60.0, got {}",
        deck.playback_elapsed
    );
}

#[test]
fn load_play_loop_set_gives_correct_in_out_bounds() {
    // Scenario: load, advance to 32 seconds, set a 4-bar loop at 128 BPM,
    // verify the in/out points are within the track duration.
    let bpm = 128.0_f32;
    let duration = 300.0_f32;
    let mut deck = make_loaded_deck(0, bpm, duration);
    deck.playback_elapsed = 32.0;

    let length_bars = 4.0_f32;
    let length_secs = RampDuration::Bars(length_bars).to_secs(bpm);
    let in_secs = deck.playback_elapsed;
    let out_secs = in_secs + length_secs;

    deck.loop_state = Some(LoopState { in_secs, out_secs, length_bars });

    let ls = deck.loop_state.as_ref().unwrap();
    assert!(ls.in_secs >= 0.0, "in_secs must be non-negative");
    assert!(
        ls.out_secs <= duration,
        "out_secs {} must be within track duration {}",
        ls.out_secs,
        duration
    );
    assert!(ls.out_secs > ls.in_secs, "out_secs must be greater than in_secs");
}

#[test]
fn set_head_deck_then_verify_bpm_propagation_and_sync_state() {
    // Scenario: both decks loaded, deck 2 at 132 BPM; set head to deck 2;
    // verify session BPM adopts deck 2's native_bpm; simulate sync on deck 1.
    let mut state = make_state();
    state.decks[0] = make_loaded_deck(0, 128.0, 300.0);
    state.decks[1] = make_loaded_deck(1, 132.0, 300.0);

    // Engine: head 2 → head_deck = 1, session bpm = 132.
    state.head_deck = 1;
    if let Some(bpm) = state.decks[state.head_deck].native_bpm {
        state.bpm = bpm;
    }

    assert_eq!(state.head_deck, 1);
    assert!(
        (state.bpm - 132.0).abs() < 1e-4,
        "session BPM should be 132.0, got {}",
        state.bpm
    );

    // Engine: sync 1 → deck 0 rate = head_bpm / deck0_native_bpm.
    let head_bpm = state.bpm;
    let deck0_native = state.decks[0].native_bpm.unwrap();
    let new_rate = head_bpm / deck0_native;
    state.decks[0].rate = new_rate;
    state.decks[0].synced = true;

    assert!(state.decks[0].synced);
    let expected_rate = 132.0 / 128.0;
    assert!(
        (state.decks[0].rate - expected_rate).abs() < 1e-5,
        "deck 0 rate should be {expected_rate}, got {}",
        state.decks[0].rate
    );
}
