/// ui.rs — Ratatui render functions for the Performative TUI.
///
/// Exports a single public entry point `render()` that lays out the full terminal
/// frame: two deck panels (left/right), a command-input line, and a status bar.
///
/// Each deck panel contains:
///   - A bordered block whose title and border color reflect deck and jog state.
///   - A track-name row with a right-aligned playback-state badge.
///   - A time-display row.
///   - An EQ/gain row.
///   - A right-facing semicircle jog platter rendered via Canvas below the content rows, with
///     arc_position (0..2π) driving clockwise rotation in Ratatui's standard math coordinates.
use performative_core::{AppState, DeckState};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, canvas::{Canvas, Line as CanvasLine}},
};

/// Render the full terminal frame.
///
/// Lays out deck panels, command input, and status bar, then delegates to
/// `render_deck()`, `render_input()`, and `render_status()`.
pub fn render(frame: &mut Frame, state: &AppState) {
    let vertical = Layout::vertical([
        Constraint::Min(9),      // deck panels (taller to fit EQ row)
        Constraint::Length(1),   // command input
        Constraint::Length(1),   // status message
    ]);
    let [decks_area, input_area, status_area] = vertical.areas(frame.area());

    let horizontal = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ]);
    let [deck1_area, deck2_area] = horizontal.areas(decks_area);

    render_deck(frame, &state.decks[0], deck1_area, state.jog_deck);
    render_deck(frame, &state.decks[1], deck2_area, state.jog_deck);
    render_input(frame, &state.input_line, input_area);
    render_status(frame, state, status_area);
}

/// Render a single deck panel.
///
/// Draws a bordered block, then three content rows (track name, time, EQ/gain)
/// at the top, with a Canvas-based semicircle jog platter filling the remaining space
/// below, rendered by `render_jog_platter()`.
///
/// When `jog_deck == Some(deck.index)` the border turns Cyan and a `◉` marker
/// appears in the title to indicate jog focus.
fn render_deck(
    frame: &mut Frame,
    deck: &performative_core::Deck,
    area: ratatui::layout::Rect,
    jog_deck: Option<usize>,
) {
    let is_jog_target = jog_deck == Some(deck.index);

    let title = if is_jog_target {
        format!(" DECK {} ◉ ", deck.index + 1)
    } else {
        format!(" DECK {} ", deck.index + 1)
    };

    let (state_label, state_color) = match deck.state {
        DeckState::Playing => ("▶ PLAYING", Color::Green),
        DeckState::Paused  => ("⏸ PAUSED",  Color::Yellow),
        DeckState::Loaded  => ("◉ LOADED",  Color::Cyan),
        DeckState::Empty   => ("○ EMPTY",   Color::DarkGray),
    };

    let border_color = if deck.state == DeckState::Playing {
        if is_jog_target { Color::Cyan } else { Color::Green }
    } else if is_jog_target {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split vertically: 3 content rows at top, remaining space for platter canvas
    let deck_layout = Layout::vertical([
        Constraint::Length(3),   // content rows (track, time, EQ)
        Constraint::Min(4),      // platter canvas (at least 4 rows for a visible circle)
    ]);
    let [content_area, platter_area] = deck_layout.areas(inner);

    let rows = Layout::vertical([
        Constraint::Length(1), // track name + state badge
        Constraint::Length(1), // time display
        Constraint::Length(1), // EQ/gain row
    ]);
    let [track_area, time_area, eq_area] = rows.areas(content_area);

    // Track name + right-aligned state badge
    let track_text = deck.track_name.as_deref().unwrap_or("—").to_string();
    let name_width = track_area.width.saturating_sub(state_label.len() as u16 + 1) as usize;
    let truncated = truncate(&track_text, name_width);
    let padding = " ".repeat(
        track_area.width.saturating_sub(truncated.len() as u16 + state_label.len() as u16) as usize
    );
    let track_line = Line::from(vec![
        Span::styled(truncated, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(padding),
        Span::styled(state_label, Style::default().fg(state_color)),
    ]);
    frame.render_widget(Paragraph::new(track_line), track_area);

    // Time display
    frame.render_widget(
        Paragraph::new(format!("  {}", deck.playhead_display()))
            .style(Style::default().fg(Color::White)),
        time_area,
    );

    // EQ / gain row
    let gain_db = linear_to_db(deck.gain);
    let eq_line = Line::from(vec![
        Span::raw("  gain:"),
        Span::styled(format!("{gain_db:+.1}dB"), val_color(deck.gain)),
        Span::raw("  lo:"),
        Span::styled(format!("{:.2}", deck.lo_gain), val_color(deck.lo_gain)),
        Span::raw("  mid:"),
        Span::styled(format!("{:.2}", deck.mid_gain), val_color(deck.mid_gain)),
        Span::raw("  hi:"),
        Span::styled(format!("{:.2}", deck.hi_gain), val_color(deck.hi_gain)),
    ]);
    frame.render_widget(Paragraph::new(eq_line), eq_area);

    render_jog_platter(frame, &deck.jog, platter_area, is_jog_target);
}

/// Render a right-facing semicircle jog arc using the Canvas widget.
///
/// Draws a right-facing ")" arc and a bright sweep segment whose position
/// is driven by `jog.arc_position` (0..π). The sweep color reflects the
/// current `JogPhase`:
///   - `Idle` (focused): Cyan   |  `Idle` (unfocused): DarkGray
///   - `Scratching`: Green
///   - `Bending`: Blue
///   - `Releasing`: Yellow
fn render_jog_platter(
    frame: &mut Frame,
    jog: &performative_core::deck::JogState,
    area: ratatui::layout::Rect,
    is_focused: bool,
) {
    use performative_core::deck::JogPhase;

    if area.width < 4 || area.height < 3 {
        return;
    }

    let phase_color = match jog.phase {
        JogPhase::Idle => {
            if is_focused { Color::Cyan } else { Color::DarkGray }
        }
        JogPhase::Scratching => Color::Green,
        JogPhase::Bending    => Color::Blue,
        JogPhase::Releasing  => Color::Yellow,
    };

    let bound = 10.0_f64;
    let radius = 6.0_f64;
    // arc_position (0..2π, wrapping) offsets the start angle of the semicircle so
    // the entire arc rotates as the platter spins. The semicircle always spans π
    // radians; only its starting angle changes. Ratatui's Canvas uses standard math
    // coordinates (+y up, increasing angle = counterclockwise), so negate the offset
    // so forward platter motion appears clockwise on screen.
    let offset = -(jog.arc_position as f64);
    let start_angle = offset;
    let end_angle = offset + std::f64::consts::PI;

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-bound, bound])
        .y_bounds([-bound, bound])
        .paint(|ctx| {
            // Draw the rotating semicircle arc as 80 connected line segments so the
            // line appears continuous. All segments share phase_color — no dial or
            // radius hand is drawn.
            let n_segments = 80_usize;
            for i in 0..n_segments {
                let t0 = i as f64 / n_segments as f64;
                let t1 = (i + 1) as f64 / n_segments as f64;
                let a0 = start_angle + t0 * (end_angle - start_angle);
                let a1 = start_angle + t1 * (end_angle - start_angle);

                let x0 = radius * a0.cos();
                let y0 = radius * a0.sin();
                let x1 = radius * a1.cos();
                let y1 = radius * a1.sin();

                ctx.draw(&CanvasLine::new(x0, y0, x1, y1, phase_color));
            }
        });

    frame.render_widget(canvas, area);
}

/// Render the command-input line.
fn render_input(frame: &mut Frame, input: &str, area: ratatui::layout::Rect) {
    frame.render_widget(
        Paragraph::new(format!("> {input}█"))
            .style(Style::default().fg(Color::White)),
        area,
    );
}

/// Render the status bar.
///
/// Shows the remaining duration of an active transition if one is in progress,
/// otherwise shows the general status message.
fn render_status(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    let msg = if let Some(ref t) = state.active_transition {
        let elapsed = t.start.elapsed().as_secs_f32();
        let remaining = (t.total_secs - elapsed).max(0.0);
        format!("{}: {remaining:.1}s remaining", t.label)
    } else {
        state.status_msg.clone()
    };
    frame.render_widget(
        Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

/// Color a gain/EQ value: red if killed (~0), green if boosted (>1), white otherwise.
fn val_color(v: f32) -> Style {
    if v < 0.01 {
        Style::default().fg(Color::Red)
    } else if v > 1.05 {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::White)
    }
}

/// Convert a linear gain value to decibels, clamped at -96 dB.
fn linear_to_db(v: f32) -> f32 {
    if v < 0.00001 { -96.0 } else { 20.0 * v.log10() }
}

/// Truncate a string to at most `max_chars` characters, adding `…` if cut.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else if max_chars > 1 {
        let end = s
            .char_indices()
            .nth(max_chars - 1)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    } else {
        String::new()
    }
}
