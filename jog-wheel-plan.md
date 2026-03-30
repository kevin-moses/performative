# Jog Wheel: Trackpad/Scroll Scrubbing with TUI Visual

## Context

Replicate real DJ jog wheel behavior using the trackpad/scroll wheel. Two modes: **vinyl scratch** (default) where scroll directly controls playback rate like a hand on vinyl, and **pitch bend** (Shift+scroll) where scroll temporarily nudges speed. Includes a visual semicircle arc in each deck panel that sweeps with scroll input.

Deck targeting: click on a deck panel to select it for jog, or use a CLI command. The feature-engineer agent must be used for all code writing.

---

## How Real Jog Wheels Work (Reference)

**Vinyl/Scratch mode**: Touching the platter couples audio to hand movement. `playback_rate = hand_velocity / normal_velocity`. Pitch and tempo shift together — no time-stretching. Touch = decelerate to stop (~100-300ms). Release = accelerate back to normal (~200-500ms). This IS the characteristic scratch sound — rapid pitch sweeps from rate changes.

**Pitch Bend mode**: `rate = base_rate + (jog_velocity × sensitivity)`. Temporary speed nudge. Returns to normal instantly when input stops. Used for beat-matching.

---

## Architecture Overview

```
Scroll/Click events (crossterm)
    │
    ▼
Rust: JogState per deck (velocity filter, state machine)
    │  ├─ IDLE: rate = base_rate
    │  ├─ BRAKING: rate decelerating toward 0 (vinyl touch)
    │  ├─ SCRATCHING: rate = filtered scroll velocity
    │  └─ RELEASING: rate accelerating back to base_rate
    │
    ▼
OSC: n_set rate + rate_lag to scsynth at ~30-60 Hz
    │
    ▼
scsynth: deck_player with VarLag.kr(rate, rate_lag) — smooth interpolation
```

---

## Sub-tasks

### J1 — SynthDef: Add `rate_lag` to deck_player

**Why**: The current SynthDef uses `rate` directly with no smoothing. Jog wheel requires smooth rate transitions to avoid zipper noise/clicking. Different lag values for different modes: scratch (~5ms), pitch bend (~50ms), spin-up/down (~300ms).

**Files**: `synthdefs/compile.scd`

Change deck_player:
```supercollider
SynthDef(\deck_player, { |buf=0, rate=1.0, rate_lag=0.0, gain=1.0, gain_lag=0.0,
                           out_bus=10, loop_=0, pos=0, gate=1|
    var sig, env, smoothRate;
    smoothRate = VarLag.kr(rate, rate_lag);
    sig = PlayBuf.ar(2, buf,
        rate: BufRateScale.kr(buf) * smoothRate,
        trigger: 1, startPos: pos, loop: loop_,
        doneAction: Done.freeSelf
    );
    env = EnvGen.kr(Env.asr(0, 1, 0.01), gate, doneAction: Done.freeSelf);
    sig = sig * VarLag.kr(gain, gain_lag) * env;
    Out.ar(out_bus, sig);
}).writeDefFile(outputDir);
```

After changing, recompile: `sclang synthdefs/compile.scd`

Also update `create_deck_synths()` in `engine.rs` to pass `("rate_lag", 0.0)` in the s_new controls (so it starts with no lag, lag is only set during jog).

---

### J2 — Jog State Model

**Why**: Need per-deck state to track jog mode, velocity, timing, and the brake/release state machine.

**Files**: `performative-core/src/deck.rs`, `performative-core/src/app_state.rs`

New types in `deck.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub enum JogMode {
    #[default]
    Vinyl,    // default: scroll = scratch
    PitchBend, // shift+scroll = nudge
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum JogPhase {
    #[default]
    Idle,       // rate = base_rate, no jog activity
    Braking,    // decelerating from base_rate toward scratch input (vinyl touch sim)
    Scratching, // rate follows scroll velocity
    Releasing,  // accelerating back to base_rate (vinyl release sim)
    Bending,    // pitch bend active (rate = base + offset)
}

#[derive(Debug, Clone)]
pub struct JogState {
    pub phase: JogPhase,
    pub mode: JogMode,
    /// Filtered scroll velocity (alpha-beta filter output). Units: rate multiplier.
    pub velocity: f32,
    /// Raw velocity estimate for the filter.
    pub raw_velocity: f32,
    /// Timestamp of last scroll event.
    pub last_event: Instant,
    /// Timestamp when current phase started (for brake/release curves).
    pub phase_start: Instant,
    /// The base rate to return to (1.0 normally, or synced rate).
    pub base_rate: f32,
    /// Current effective rate (sent to scsynth).
    pub effective_rate: f32,
    /// Accumulated position offset for the arc visual (wraps 0..2π).
    pub arc_position: f32,
    /// Alpha-beta filter state: position estimate.
    pub filter_pos: f32,
    /// Alpha-beta filter state: velocity estimate.
    pub filter_vel: f32,
}
```

`JogState::new()` initializes with `phase: Idle`, `base_rate: 1.0`, `effective_rate: 1.0`.

Add to `Deck`:
```rust
pub jog: JogState,
```

Add to `AppState`:
```rust
pub jog_deck: Option<usize>,  // which deck is jog-focused, None = neither
```

---

### J3 — Jog Engine: Velocity Filter + State Machine

**Why**: Core logic that converts raw scroll deltas into smooth rate values and manages the brake/release lifecycle.

**Files**: New file `performative-core/src/jog.rs`, modify `performative-core/src/lib.rs` to add `pub mod jog`

#### Alpha-beta filter

Converts discrete scroll deltas into smooth velocity:

```rust
const ALPHA: f32 = 0.125;  // position smoothing (1/8)
const BETA: f32 = 0.004;   // velocity smoothing (alpha/32)

pub fn filter_update(jog: &mut JogState, delta: f32, dt: f32) {
    // Predict
    let predicted_pos = jog.filter_pos + jog.filter_vel * dt;
    // Update
    let residual = delta - predicted_pos;
    jog.filter_pos = predicted_pos + ALPHA * residual;
    jog.filter_vel += BETA * residual / dt;
    jog.velocity = jog.filter_vel;
}
```

#### Scroll event handler

Called when a scroll event arrives:

```rust
pub fn on_scroll(jog: &mut JogState, delta: i32, shift_held: bool) {
    let now = Instant::now();
    let dt = now.duration_since(jog.last_event).as_secs_f32().max(0.001);
    jog.last_event = now;

    let scroll_delta = delta as f32 * SCROLL_SENSITIVITY;

    if shift_held {
        // Pitch bend mode
        jog.mode = JogMode::PitchBend;
        filter_update(jog, scroll_delta, dt);
        jog.phase = JogPhase::Bending;
        jog.effective_rate = jog.base_rate + jog.velocity * BEND_SENSITIVITY;
    } else {
        // Vinyl scratch mode
        jog.mode = JogMode::Vinyl;
        match jog.phase {
            JogPhase::Idle => {
                jog.phase = JogPhase::Braking;
                jog.phase_start = now;
                // Reset filter
                jog.filter_pos = 0.0;
                jog.filter_vel = 0.0;
            }
            _ => {}
        }
        filter_update(jog, scroll_delta, dt);

        // Transition to scratching once we have input
        if jog.phase == JogPhase::Braking {
            jog.phase = JogPhase::Scratching;
        }
        jog.effective_rate = jog.velocity * SCRATCH_SENSITIVITY;
    }

    // Update arc visual position
    jog.arc_position = (jog.arc_position + scroll_delta * ARC_SENSITIVITY) % (2.0 * std::f32::consts::PI);
    if jog.arc_position < 0.0 { jog.arc_position += 2.0 * std::f32::consts::PI; }
}
```

#### Tick update (called every 33ms from event loop)

Handles state transitions when scrolling stops:

```rust
const SCROLL_TIMEOUT_MS: u64 = 80;       // ms with no scroll = "released"
const BRAKE_DURATION: f32 = 0.15;         // seconds to decelerate on vinyl touch
const RELEASE_DURATION: f32 = 0.35;       // seconds to spin back up
const BEND_DECAY: f32 = 0.85;             // per-tick multiplier for pitch bend decay

pub fn tick(jog: &mut JogState) {
    let since_last = jog.last_event.elapsed().as_millis() as u64;

    match jog.phase {
        JogPhase::Idle => {
            // During idle, arc position advances with base_rate for visual spinning
            jog.arc_position = (jog.arc_position + jog.base_rate * 0.05) % (2.0 * std::f32::consts::PI);
        }
        JogPhase::Scratching => {
            if since_last > SCROLL_TIMEOUT_MS {
                // No scroll input — start releasing (spin back up)
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
                // Ease from last scratch velocity toward base_rate
                let from = jog.velocity * SCRATCH_SENSITIVITY;
                jog.effective_rate = from + (jog.base_rate - from) * ease_out(t);
            }
        }
        JogPhase::Braking => {
            let t = jog.phase_start.elapsed().as_secs_f32() / BRAKE_DURATION;
            if t >= 1.0 {
                jog.effective_rate = 0.0;
                jog.phase = JogPhase::Scratching;
            } else {
                jog.effective_rate = jog.base_rate * (1.0 - ease_out(t));
            }
        }
        JogPhase::Bending => {
            if since_last > SCROLL_TIMEOUT_MS {
                // Decay the velocity offset back to 0
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

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(2)  // quadratic ease-out
}
```

#### Rate sending

In the tick loop, if `jog.phase != Idle`, send the rate to scsynth:

```rust
pub fn rate_and_lag(jog: &JogState) -> (f32, f32) {
    let lag = match jog.phase {
        JogPhase::Idle => 0.0,
        JogPhase::Scratching => 0.005,      // 5ms — tight, responsive
        JogPhase::Braking => 0.05,           // 50ms — smooth decel
        JogPhase::Releasing => 0.05,         // 50ms — smooth accel
        JogPhase::Bending => 0.03,           // 30ms — smooth nudge
    };
    (jog.effective_rate, lag)
}
```

Constants to tune:
```rust
const SCROLL_SENSITIVITY: f32 = 0.15;  // raw scroll delta → filter input
const SCRATCH_SENSITIVITY: f32 = 2.0;  // filter velocity → rate multiplier
const BEND_SENSITIVITY: f32 = 0.08;    // filter velocity → rate offset
const ARC_SENSITIVITY: f32 = 0.3;      // scroll delta → arc position change
```

---

### J4 — Mouse Capture + Event Handling

**Why**: Need to capture mouse clicks (deck selection) and scroll events (jog input).

**Files**: `performative-tui/src/app.rs`

#### Enable mouse capture

In `run_tui()`:
```rust
use crossterm::event::{EnableMouseCapture, DisableMouseCapture, MouseEventKind};

// On startup:
execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

// On cleanup:
execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;

// Also in the panic hook:
let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
```

#### Handle mouse events in `handle_event()`

Add a new match arm:

```rust
Event::Mouse(mouse_event) => {
    match mouse_event.kind {
        MouseEventKind::Down(_) => {
            // Click on deck panel — determine which deck based on column position
            let mid = terminal_width / 2;  // need to pass or compute frame width
            let deck_idx = if mouse_event.column < mid { 0 } else { 1 };
            state.lock().await.jog_deck = Some(deck_idx);
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta = if matches!(mouse_event.kind, MouseEventKind::ScrollUp) { 1 } else { -1 };
            let shift = mouse_event.modifiers.contains(KeyModifiers::SHIFT);
            let mut st = state.lock().await;
            if let Some(deck_idx) = st.jog_deck {
                jog::on_scroll(&mut st.decks[deck_idx].jog, delta, shift);
            }
        }
        _ => {}
    }
}
```

#### Jog tick in the event loop

In the tick handler (the 33ms interval), after drawing the frame, process jog state and send OSC:

```rust
_ = ticker.tick() => {
    // Jog tick — update state machine and send rate to scsynth
    let mut st = state.lock().await;
    if let Some(deck_idx) = st.jog_deck {
        let was_active = st.decks[deck_idx].jog.phase != JogPhase::Idle;
        jog::tick(&mut st.decks[deck_idx].jog);
        let phase = st.decks[deck_idx].jog.phase.clone();
        let (rate, lag) = jog::rate_and_lag(&st.decks[deck_idx].jog);

        if phase != JogPhase::Idle || was_active {
            let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
            drop(st); // release lock before async send
            let _ = engine.osc.send(msg::n_set(player_node, &[
                ("rate_lag", lag),
                ("rate", rate),
            ])).await;
        } else {
            drop(st);
        }
    } else {
        drop(st);
    }
    let snap = state.lock().await.clone();
    terminal.draw(|f| ui::render(f, &snap))?;
}
```

---

### J5 — Parser: `jog` CLI Command

**Why**: User wants CLI command to select jog deck (alternative to clicking).

**Files**: `performative-parser/src/lib.rs`, `performative-core/src/engine.rs`

```rust
Command::Jog { deck: usize }
```

Add `"jog"` to `parse()` match. NOT in `SINGLE_DECK_VERBS` (always takes explicit deck).

```rust
"jog" => Ok(Command::Jog { deck: parse_deck(tokens.get(1), "jog")? }),
```

Engine:
```rust
Command::Jog { deck } => {
    self.state.lock().await.jog_deck = Some(deck);
    self.state.lock().await.status_msg = format!("Jog: Deck {}", deck + 1);
    Ok(())
}
```

---

### J6 — TUI Visual: Vertical Arc + Jog-Focus Indicator

**Why**: Visual feedback showing jog activity. A vertical right-facing semicircle arc (like `)`) with a filled sweep that rotates to show the jog position. Plus a small indicator showing which deck is jog-focused.

**Files**: `performative-tui/src/ui.rs`

#### Arc design

The arc is a vertical right-facing open semicircle, rendered using Unicode box-drawing / arc characters. It takes 5 rows × 3-4 columns on the right side of each deck panel. The "sweep" (filled portion) moves around the arc based on `jog.arc_position`. Two layers: a dim background arc (track) and a bright foreground sweep (position).

```
Idle (dot orbiting slowly):      Scratching (sweep follows input):

    ╮                                ┃
    │                                │
    │                                │
    │                                ┃
    ╯                                ╯

Background arc (dim):   ╮│││╯      (always visible, DarkGray)
Sweep/filled (bright):  ┃          (colored segment, sweeps around)
```

The arc characters, top to bottom: `╮`, `│`, `│`, `│`, `╯`. The sweep replaces a contiguous segment with bright colored characters: `┃` (thick vertical) for the swept portion.

Render approach: the arc has 5 character positions (top to bottom). `arc_position` (0.0–1.0) maps to which positions are "swept". The sweep is a ~40% window that slides around:

```rust
fn render_jog_arc(frame: &mut Frame, jog: &JogState, area: Rect, is_focused: bool) {
    // 5 arc positions: indices 0-4 map to the semicircle top→bottom
    let arc_chars = ["╮", "│", "│", "│", "╯"];
    let sweep_chars = ["╗", "┃", "┃", "┃", "╝"];

    let sweep_center = (jog.arc_position * 5.0) % 5.0;  // 0..5
    let sweep_radius = 1.0; // ±1 position around center = 3 chars swept

    let sweep_color = match jog.phase {
        JogPhase::Idle => if is_focused { Color::Cyan } else { Color::DarkGray },
        JogPhase::Scratching => Color::Green,
        JogPhase::Bending => Color::Blue,
        JogPhase::Braking | JogPhase::Releasing => Color::Yellow,
    };
    let bg_color = Color::DarkGray;

    for i in 0..5 {
        let dist = ((i as f32 - sweep_center).abs()).min((i as f32 - sweep_center + 5.0).abs())
                     .min((i as f32 - sweep_center - 5.0).abs());
        let (ch, color) = if dist <= sweep_radius {
            (sweep_chars[i], sweep_color)
        } else {
            (arc_chars[i], bg_color)
        };
        // Render at area.right()-1, area.top()+i
        frame.render_widget(
            Paragraph::new(ch).style(Style::default().fg(color)),
            Rect::new(area.right().saturating_sub(2), area.top() + i as u16, 1, 1),
        );
    }
}
```

#### Deck panel layout changes

Reserve 2 columns on the right of the deck inner area for the arc. The existing content renders in `width - 2` columns. The arc renders in the rightmost 2 columns, spanning the full inner height (5 rows).

```rust
let inner = block.inner(area);
let content_area = Rect::new(inner.x, inner.y, inner.width.saturating_sub(2), inner.height);
let arc_area = Rect::new(inner.right().saturating_sub(2), inner.y, 2, inner.height.min(5));
// Render text content in content_area, arc in arc_area
```

#### Jog-focus indicator

Small indicator in the deck panel title bar. When a deck is jog-focused, show a dot in the title:

```rust
let title = if is_jog_target {
    format!(" DECK {} ◉ ", deck.index + 1)  // ◉ = jog active
} else {
    format!(" DECK {} ", deck.index + 1)
};
```

The `◉` is small, unobtrusive, and immediately tells the user which deck is jog-selected.

Also change the border color for the focused deck:

```rust
let is_jog_target = jog_deck == Some(deck.index);
let border_color = if deck.state == DeckState::Playing {
    if is_jog_target { Color::Cyan } else { Color::Green }
} else if is_jog_target {
    Color::Cyan
} else {
    Color::DarkGray
};
```

#### Passing jog_deck to render_deck

Update `render()` to pass `state.jog_deck` to `render_deck()`. Update `render_deck()` signature to accept `jog_deck: Option<usize>`.

---

## Execution Order (Feature-Engineer Agent Tasks)

All code writing MUST use the feature-engineer agent. Grouped into 3 agent invocations:

**Task A** (feature-engineer): J1 + J2 — SynthDef `rate_lag` + jog state model types
- Modify `compile.scd`, recompile SynthDefs
- Add `JogMode`, `JogPhase`, `JogState` to `deck.rs`
- Add `jog_deck` to `app_state.rs`
- Add `jog` field to `Deck`
- Update `create_deck_synths()` to pass `rate_lag`

**Task B** (feature-engineer): J3 + J4 + J5 — Jog engine + mouse handling + CLI command
- Create `jog.rs` with filter, `on_scroll()`, `tick()`, `rate_and_lag()`
- Enable mouse capture in `app.rs`
- Handle scroll + click events
- Add jog tick + OSC send in event loop
- Add `Command::Jog` to parser + engine dispatch

**Task C** (feature-engineer): J6 — TUI vertical arc visual + jog-focus indicator
- `render_jog_arc()` function in `ui.rs`
- Deck panel layout changes (reserve arc column)
- `◉` indicator in title bar for focused deck
- Border color change for jog-focused deck

---

## Files Changed Summary

| File | Changes |
|---|---|
| `synthdefs/compile.scd` | Add `rate_lag` param to deck_player, wrap rate in `VarLag.kr` |
| `performative-core/src/deck.rs` | `JogMode`, `JogPhase`, `JogState` structs; `jog` field on Deck |
| `performative-core/src/jog.rs` (new) | Alpha-beta filter, `on_scroll()`, `tick()`, `rate_and_lag()`, constants |
| `performative-core/src/lib.rs` | Add `pub mod jog` |
| `performative-core/src/app_state.rs` | `jog_deck: Option<usize>` field |
| `performative-core/src/engine.rs` | Pass `rate_lag` in `create_deck_synths()`; `Command::Jog` dispatch |
| `performative-parser/src/lib.rs` | `Command::Jog { deck }` variant + parse function |
| `performative-tui/src/app.rs` | `EnableMouseCapture`/`DisableMouseCapture`; mouse event handling; jog tick + OSC send |
| `performative-tui/src/ui.rs` | `render_jog_arc()` function; new deck panel row; jog-focus border highlight |

---

## Tuning Constants

These will need E2E tuning with real audio:

| Constant | Starting Value | Purpose |
|---|---|---|
| `SCROLL_SENSITIVITY` | 0.15 | Raw scroll delta → filter input scale |
| `SCRATCH_SENSITIVITY` | 2.0 | Filter velocity → playback rate |
| `BEND_SENSITIVITY` | 0.08 | Filter velocity → rate offset |
| `BRAKE_DURATION` | 0.15s | Time to decelerate on first scroll (vinyl touch) |
| `RELEASE_DURATION` | 0.35s | Time to spin back up after scrolling stops |
| `SCROLL_TIMEOUT_MS` | 80ms | No-scroll duration before "release" triggers |
| `ALPHA` | 0.125 | Alpha-beta filter: position smoothing |
| `BETA` | 0.004 | Alpha-beta filter: velocity smoothing |
| `BEND_DECAY` | 0.85 | Per-tick decay for pitch bend (at 30fps) |

---

## Verification

```bash
# After J1: recompile SynthDef
cd synthdefs && sclang compile.scd

# After each sub-task:
cargo build && cargo test

# E2E test:
# 1. cargo run
# 2. load 1 ~/track.wav
# 3. play 1
# 4. jog 1            → "Jog: Deck 1"
# 5. Scroll up/down   → audio scratches (rate follows scroll speed)
#                     → arc dot sweeps with scroll
#                     → rate snaps back when scrolling stops
# 6. Shift+scroll     → pitch bends (temporary speed nudge)
#                     → "BEND +N%" indicator
# 7. Click deck 2     → jog switches to deck 2
# 8. Scroll on deck 2 → deck 2 scratches, deck 1 unaffected
```
