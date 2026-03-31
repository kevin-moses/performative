use anyhow::Result;
use crossterm::{
    event::{
        EnableMouseCapture, DisableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use performative_core::{AudioEngine, AppState};
use performative_core::deck::{DeckState, JogPhase};
use performative_core::jog;
use performative_osc::messages as msg;
use performative_parser::{ParseError, parse_script};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, path::PathBuf, sync::Arc};
use tokio::{sync::Mutex, time::{Duration, interval}};

use crate::ui;

pub async fn run_tui() -> Result<()> {
    // Restore terminal on panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn run_inner(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let state = Arc::new(Mutex::new(AppState::new()));
    let engine = Arc::new(AudioEngine::new(state.clone()).await?);

    // Boot scsynth and load synthdefs — updates status_msg while running.
    let _scsynth = engine.boot_scsynth().await?;
    state.lock().await.status_msg = "Ready. Type a command.".into();

    // Start the background loop monitor (seeks decks back to loop in_secs when
    // the playhead crosses out_secs). Must be spawned after scsynth is booted.
    engine.spawn_loop_monitor();

    let mut ticker = interval(Duration::from_millis(33)); // ~30 fps
    let mut events = EventStream::new();
    let mut history: Vec<String> = load_history();
    let mut hist_pos: Option<usize> = None; // Some(i) = browsing history[i]
    let mut saved_input = String::new();    // input draft saved when browsing starts

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Jog tick — advance state machine and send updated rate to scsynth.
                {
                    let mut st = state.lock().await;

                    // Advance rate-adjusted playhead for all playing decks, then check
                    // whether playback has reached the end of the loaded track.
                    for i in 0..2 {
                        if st.decks[i].state == DeckState::Playing {
                            let rate = if st.jog_deck == Some(i) && st.decks[i].jog.phase != JogPhase::Idle {
                                st.decks[i].jog.effective_rate
                            } else {
                                1.0
                            };
                            st.decks[i].advance_playhead(0.033, rate);

                            // When the playhead reaches the track duration, stop the deck
                            // so the jog arc stops rotating and the state badge updates.
                            if let Some(dur) = st.decks[i].track_duration {
                                if st.decks[i].playback_elapsed >= dur {
                                    st.decks[i].state = DeckState::Loaded;
                                    st.status_msg = format!("Deck {} finished", i + 1);
                                }
                            }
                        }
                    }

                    // Capture whether the focused deck's jog was active before ticking,
                    // so we can send a final OSC reset when it transitions to Idle.
                    let focused_was_active = st.jog_deck.map_or(false, |idx| {
                        st.decks[idx].jog.phase != JogPhase::Idle
                    });

                    // Tick all decks' jog arc positions — both should spin when playing.
                    for i in 0..2 {
                        let playing = st.decks[i].state == DeckState::Playing;
                        jog::tick(&mut st.decks[i].jog, playing);
                    }

                    // Send OSC rate updates for the focused deck when jog is active
                    // (or just became idle — one final send resets rate to base_rate).
                    if let Some(deck_idx) = st.jog_deck {
                        if st.decks[deck_idx].jog.phase != JogPhase::Idle || focused_was_active {
                            let (rate, lag) = jog::rate_and_lag(&st.decks[deck_idx].jog);
                            let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
                            drop(st);
                            let _ = engine.osc.send(msg::n_set(player_node, &[
                                ("rate_lag", lag),
                                ("rate", rate),
                            ])).await;
                        }
                    }
                }
                // Clone state so we don't hold the lock during Ratatui's draw.
                let snap = state.lock().await.clone();
                terminal.draw(|f| ui::render(f, &snap))?;
            }
            Some(Ok(event)) = events.next() => {
                if handle_event(
                    event, &state, &engine,
                    &mut history, &mut hist_pos, &mut saved_input,
                ).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `true` when the app should quit.
async fn handle_event(
    event: Event,
    state: &Arc<Mutex<AppState>>,
    engine: &Arc<AudioEngine>,
    history: &mut Vec<String>,
    hist_pos: &mut Option<usize>,
    saved_input: &mut String,
) -> Result<bool> {
    match event {
        Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
            if modifiers.contains(KeyModifiers::CONTROL) =>
        {
            return Ok(true);
        }
        Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
            return Ok(true);
        }
        Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
            let line = {
                let mut st = state.lock().await;
                let l = st.input_line.trim().to_string();
                st.input_line.clear();
                l
            };
            *hist_pos = None;
            if !line.is_empty() {
                // Avoid consecutive duplicates (same UX as most shells).
                if history.last().map(|s| s.as_str()) != Some(&line) {
                    history.push(line.clone());
                    // Keep last 100 entries and persist.
                    if history.len() > 100 {
                        history.drain(..history.len() - 100);
                    }
                    save_history(history);
                }
                dispatch(&line, engine, state).await;
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
            if history.is_empty() { return Ok(false); }
            match *hist_pos {
                None => {
                    // Save whatever the user was typing, jump to newest entry.
                    let current = state.lock().await.input_line.clone();
                    *saved_input = current;
                    *hist_pos = Some(history.len() - 1);
                }
                Some(0) => {} // already at oldest, do nothing
                Some(i) => {
                    *hist_pos = Some(i - 1);
                }
            }
            if let Some(i) = *hist_pos {
                state.lock().await.input_line = history[i].clone();
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
            match *hist_pos {
                None => {} // not browsing, nothing to do
                Some(i) if i + 1 >= history.len() => {
                    // Past the newest entry — restore draft.
                    *hist_pos = None;
                    state.lock().await.input_line = saved_input.clone();
                }
                Some(i) => {
                    *hist_pos = Some(i + 1);
                    state.lock().await.input_line = history[i + 1].clone();
                }
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
            state.lock().await.input_line.pop();
        }
        Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, .. })
            if !modifiers.contains(KeyModifiers::CONTROL) =>
        {
            state.lock().await.input_line.push(c);
        }
        Event::Mouse(mouse_event) => {
            match mouse_event.kind {
                MouseEventKind::Down(_) => {
                    // Click on deck panel — left half = deck 1, right half = deck 2.
                    let mut st = state.lock().await;
                    let mid = crossterm::terminal::size().map(|(w, _)| w / 2).unwrap_or(40);
                    let deck_idx = if mouse_event.column < mid { 0 } else { 1 };
                    st.jog_deck = Some(deck_idx);
                    st.status_msg = format!("Jog: Deck {}", deck_idx + 1);
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let delta = if matches!(mouse_event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                    let shift = mouse_event.modifiers.contains(KeyModifiers::SHIFT);
                    let mut st = state.lock().await;
                    if let Some(deck_idx) = st.jog_deck {
                        jog::on_scroll(&mut st.decks[deck_idx].jog, delta, shift);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(false)
}

fn history_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".performative").join("history"))
}

fn load_history() -> Vec<String> {
    let Some(path) = history_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    text.lines().filter(|l| !l.is_empty()).map(String::from).collect()
}

fn save_history(history: &[String]) {
    let Some(path) = history_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, history.join("\n") + "\n");
}

/// Dispatch the user's input line as a composition script.
///
/// Parses the full line with `parse_script` (which handles `;`, `>`, `&`, and `N |` contexts)
/// then hands the resulting `Script` to the engine for execution. Parse errors are written to
/// the status message; empty input is silently ignored.
///
/// purpose: translate a REPL input line into engine execution.
/// @param line: (&str) the raw input line from the user
/// @param engine: (&Arc<AudioEngine>) the audio engine
/// @param state: (&Arc<Mutex<AppState>>) shared application state for error reporting
async fn dispatch(line: &str, engine: &Arc<AudioEngine>, state: &Arc<Mutex<AppState>>) {
    match parse_script(line) {
        Ok(script) => {
            if let Err(e) = engine.clone().execute_script(script, line.to_string()).await {
                state.lock().await.status_msg = format!("error: {e}");
            }
        }
        Err(ParseError::Empty) => {}
        Err(e) => {
            state.lock().await.status_msg = e.to_string();
        }
    }
}
