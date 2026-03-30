# Ticket 5: Sync, Cue, Loop

## Context

Add `seek`, `cue`, `loop`, `pre`, `head`, and `sync` commands — the core DJ workflow. This ticket transforms Performative from an audio player with EQ transitions into a functional DJ tool with beat-matched mixing, headphone preview, and hot cues.

**Acceptance criteria** (from tickets.txt):
```
Load two tracks with different BPMs. sync 2 — deck 2 plays at deck 1's BPM, beats aligned.
cue 2 set A — set a cue point. loop 2 4bars — deck 2 loops.
pre 2 — hear deck 2 in headphones only. seek 2 bar 16 — jump to bar 16.
```

**Implementation constraint**: The feature-engineer agent must be used for all code writing.

---

## Dependency Graph

```
T5.1  Deck Model + Buffer Info  (foundation)
  │
  ├─► T5.2  Seek            ─┬─► T5.3  Cue
  │                           └─► T5.4  Loop
  ├─► T5.5  Head
  └─► T5.6  Pre

T5.7  Analysis Crate        ─► T5.8  Sync

T5.9  TUI Updates            (after all above)
```

Parallel opportunities: T5.2+T5.6 can run together. T5.3+T5.4+T5.7 can run together after T5.2.

---

## T5.1 — Deck Model Expansion + Buffer Info

**Why**: Every subsequent task needs buffer metadata (sample_rate, num_frames) and per-deck state (cue points, loop, BPM).

**Files**: `deck.rs`, `engine.rs`, `app_state.rs`

### deck.rs additions

```rust
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct BufferInfo {
    pub num_frames: i32,
    pub num_channels: i32,
    pub sample_rate: f32,
}

impl BufferInfo {
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate > 0.0 { self.num_frames as f32 / self.sample_rate } else { 0.0 }
    }
    pub fn secs_to_frames(&self, secs: f32) -> i32 { (secs * self.sample_rate) as i32 }
}

#[derive(Debug, Clone)]
pub struct LoopState {
    pub in_secs: f32,
    pub out_secs: f32,
    pub length_bars: f32,
}
```

New Deck fields:
```rust
pub buffer_info: Option<BufferInfo>,
pub native_bpm: Option<f32>,
pub playing_bpm: Option<f32>,
pub rate: f32,                       // 1.0 default
pub cue_points: HashMap<char, f32>,  // label -> seconds
pub loop_state: Option<LoopState>,
pub cue_active: bool,
pub synced: bool,
```

Add `playhead_secs_f32(&self) -> f32` — returns float seconds accounting for `self.rate` (buffer advances faster/slower than wall clock when rate != 1.0).

Update `reset_playback()`: reset rate=1.0, loop_state=None, synced=false; **preserve** cue_points (per-track, not per-session).

### app_state.rs additions

```rust
pub head_deck: usize,         // 0 default
pub cue_mix_up: bool,         // false default
pub cue_deck: Option<usize>,  // None default
pub cue_blend: f32,           // 0.0 default
```

### engine.rs — buffer info after load

After `b_allocRead` succeeds in `load()`, send `b_query` and parse `/b_info` reply:
```
/b_info args: [buf_num: Int, num_frames: Int, num_channels: Int, sample_rate: Float]
```
Store as `deck.buffer_info = Some(BufferInfo { ... })`.

### Tests
- `BufferInfo::duration_secs()`, `secs_to_frames()` with known values
- `playhead_secs_f32()` accounts for rate != 1.0
- `reset_playback()` preserves cue_points, clears loop_state

---

## T5.2 — Seek Command

**Why**: Foundation for cue (jump to saved point) and loop (jump back to loop start). PlayBuf's `pos` is only read at synth creation, so seek = free player + create new one with target pos.

**Files**: `parser/lib.rs`, `engine.rs`

### Parser

```rust
Command::Seek { deck: usize, position: SeekPosition }

pub enum SeekPosition {
    Seconds(f32),         // seek 2 1:30  or  seek 2 90s
    Bar(f32),             // seek 2 bar 16
    RelativeBars(f32),    // seek 2 +4bars  or  seek 2 -8bars
    RelativeSeconds(f32), // seek 2 +30s  or  seek 2 -30s
}
```

Add `"seek"` to `parse()` match and `SINGLE_DECK_VERBS`.

Parse function handles: `M:SS` format, `bar N`, `±Nbars`, `±Ns`.

### Engine — `seek(&self, deck_idx, position)`

1. Resolve position to target_secs (clamp to 0..buffer duration)
2. Convert to frames: `buf_info.secs_to_frames(target_secs)`
3. `n_free(player_node)` — kill old player
4. `s_new("deck_player", player_node, 2/*addBefore*/, eq_node, ...)` — new player with `pos = target_frames`, current `rate` and `gain`
5. If deck was NOT playing, immediately `n_run(player_node, false)`
6. Update `elapsed_before = Duration::from_secs_f32(target_secs / rate)`, reset `play_start`

Note: deck_eq stays alive on the bus — only the player is recreated.

### Tests
- `seek 2 1:30` → Seconds(90.0)
- `seek 2 bar 16` → Bar(16.0)
- `seek 2 +4bars` → RelativeBars(4.0)
- `seek 2 -8bars` → RelativeBars(-8.0)
- Missing position → error
- Pipe injection: `2 | seek bar 16`

---

## T5.3 — Cue Command

**Why**: Set and recall named hot cue points (A–D). Persist per-track across sessions.

**Files**: `parser/lib.rs`, `engine.rs`

### Parser

```rust
Command::Cue { deck: usize, action: CueAction }

pub enum CueAction {
    Set(char),   // cue 2 set A
    Jump(char),  // cue 2 A
}
```

Add `"cue"` to `parse()` and `SINGLE_DECK_VERBS`. Labels must be A–D.

### Engine — `cue(&self, deck_idx, action)`

- **Set**: store `deck.cue_points.insert(label, playhead_secs_f32())`, persist to `~/.performative/cache/<hash>/cues.json`
- **Jump**: look up position, call `self.seek(deck_idx, SeekPosition::Seconds(pos))`

### Persistence

Use same hash function as `transcode_if_needed` (DefaultHasher on canonical path). Write JSON: `{"A": 32.5, "B": 96.0}`. Load in `engine.load()` after buffer loads (if cache file exists).

Add `serde = "1"` + `serde_json = "1"` to performative-core/Cargo.toml.

### Tests
- `cue 2 set A` → Set('A')
- `cue 2 A` → Jump('A')
- `cue 2 set E` → error (A–D only)
- Pipe injection: `2 | cue set A`

---

## T5.4 — Loop Command

**Why**: Loop a section of a track. Client-side monitoring detects when playhead crosses `loop_out` and seeks back.

**Files**: `parser/lib.rs`, `engine.rs`, `app.rs`

### Parser

```rust
Command::Loop { deck: usize, action: LoopAction }

pub enum LoopAction {
    Set(RampDuration),  // loop 2 4bars
    Off,                // loop 2 off
    Halve,              // loop 2 halve
    Double,             // loop 2 double
}
```

Add `"loop"` to `parse()` and `SINGLE_DECK_VERBS`.

### Engine — `set_loop(&self, deck_idx, action)`

- **Set**: `in_secs = playhead_secs_f32()`, `out_secs = in_secs + duration.to_secs(bpm)`, store as `LoopState`
- **Off**: clear `loop_state`
- **Halve**: halve `out_secs - in_secs`, update `length_bars /= 2.0`
- **Double**: double the length

### Loop monitor — `spawn_loop_monitor(self: &Arc<Self>)`

Spawned after `boot_scsynth()` in `run_inner()`. Runs at ~33ms interval:
```rust
for deck_idx in 0..2 {
    if deck.state == Playing && deck.loop_state.is_some() {
        if playhead_secs_f32() >= loop_state.out_secs {
            seek(deck_idx, SeekPosition::Seconds(loop_state.in_secs))
        }
    }
}
```

### Tests
- `loop 2 4bars` → Set(Bars(4.0))
- `loop 2 off` → Off
- `loop 2 halve` / `loop 2 double`
- Pipe injection: `2 | loop 4bars`

---

## T5.5 — Head Command

**Why**: Set reference deck for BPM/scheduling. Simple and quick.

**Files**: `parser/lib.rs`, `engine.rs`

### Parser

```rust
Command::Head { deck: usize }
```

Add `"head"` to `parse()`. NOT in `SINGLE_DECK_VERBS` (always explicit).

### Engine — `set_head(&self, deck_idx)`

Set `st.head_deck = deck_idx`. If deck has `native_bpm`, update `st.bpm`.

### Tests
- `head 2` → Head { deck: 1 }
- `head 3` → error

---

## T5.6 — Pre Command (Cue Bus Routing)

**Why**: Route a deck to hardware output 2–3 for headphone preview. The `cue_mix` SynthDef already exists and is compiled.

**Files**: `parser/lib.rs`, `engine.rs`

### Parser

```rust
Command::Pre { action: PreAction }

pub enum PreAction {
    Deck(usize),   // pre 2
    Off,           // pre off
    Blend(f32),    // pre blend 0.5
}
```

Add `"pre"` to `parse()`. NOT in `SINGLE_DECK_VERBS` (pre off/blend have no deck).

### Engine — `pre(&self, action)`

- **Deck(idx)**: if `!cue_mix_up`, create `cue_mix` synth (addToTail, node 121) with `cue_bus` = deck's bus. Otherwise `n_set` to change `cue_bus`.
- **Off**: `n_free(CUE_MIX_NODE)`, set `cue_mix_up = false`
- **Blend(v)**: `n_set(CUE_MIX_NODE, [("blend", v)])`

### Boot change

Add `"cue_mix"` to the SynthDef d_load list in `boot_scsynth()`:
```rust
for def in &["deck_player", "deck_eq", "master_mix", "cue_mix"] {
```

### Tests
- `pre 2` → Deck(1)
- `pre off` → Off
- `pre blend 0.5` → Blend(0.5)

---

## T5.7 — Analysis Crate

**Why**: BPM/key detection via Python/Essentia. Required for sync to be meaningful.

**Files**: New crate `performative-analysis/` + `scripts/analyze.py`

### Crate structure

```
crates/performative-analysis/
  Cargo.toml   (serde, serde_json, tokio, anyhow)
  src/lib.rs
```

Add to workspace members in root Cargo.toml. Add as dependency of performative-core.

### Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackAnalysis {
    pub bpm: f32,
    pub key: String,
    pub beats: Vec<f32>,
    pub downbeats: Vec<f32>,
    pub duration_secs: f32,
}
```

### `analyze(track_path: &str) -> Result<TrackAnalysis>`

1. Check cache at `~/.performative/cache/<hash>/analysis.json`
2. If miss: shell out to `python3 scripts/analyze.py <path>`, parse JSON stdout
3. Cache result

### Python script (`scripts/analyze.py`)

Uses `essentia.standard`: `RhythmExtractor2013` for BPM/beats, `KeyExtractor` for key. Outputs JSON to stdout. Downbeats estimated as every 4th beat.

### Engine integration

In `load()`, after buffer loads, spawn background analysis task. Non-blocking — deck is playable immediately. When complete, sets `deck.native_bpm` and updates `st.bpm` if head deck. If Essentia not installed, warns in status_msg but continues.

### Tests
- `TrackAnalysis` serde roundtrip
- `analysis_cache_path()` produces valid path
- Graceful error when Python/Essentia missing

---

## T5.8 — Sync Command

**Why**: Rate-based BPM matching. `sync 2` makes deck 2 play at head deck's BPM via `rate = head_bpm / native_bpm`. Pitch changes proportionally (Rubberband deferred).

**Files**: `parser/lib.rs`, `engine.rs`

### Parser

```rust
Command::Sync { deck: usize, enabled: bool }
```

- `sync 2` → enabled: true
- `sync 2 off` → enabled: false

Add `"sync"` to `parse()` and `SINGLE_DECK_VERBS`.

### Engine — `sync(&self, deck_idx, enabled)`

- **Enable**: require `deck.native_bpm` (error if analysis not done). Calculate `rate = head_bpm / native_bpm`. Warn if >15% stretch. `n_set(player, [("rate", rate)])` if synths up. Set `deck.rate`, `deck.playing_bpm`, `deck.synced`.
- **Disable**: `n_set(player, [("rate", 1.0)])`. Reset rate/playing_bpm/synced.

Also: update `create_deck_synths()` to use `deck.rate` instead of hardcoded `1.0`.

### Tests
- `sync 2` → Sync { deck: 1, enabled: true }
- `sync 2 off` → Sync { deck: 1, enabled: false }
- Pipe: `2 | sync`

---

## T5.9 — TUI Updates

**Why**: Display all new state in deck panels and status bar.

**Files**: `ui.rs`

### Deck panel changes

Add a 4th row for indicators. Increase deck area minimum height.

- **Row 2** (time): add BPM display + "(synced)" badge if synced
- **Row 4** (new): cue point markers `[A] [B]` (magenta), loop indicator `LOOP 4bars` (cyan), cue routing `CUE` (yellow)

### Status bar

Show head deck indicator and cue routing info alongside existing status_msg.

---

## Files Changed Summary

| File | Changes |
|---|---|
| `performative-parser/src/lib.rs` | 6 new Command variants + enums (SeekPosition, CueAction, LoopAction, PreAction), 6 parse functions, SINGLE_DECK_VERBS expanded, tests |
| `performative-core/src/deck.rs` | BufferInfo, LoopState structs; 8 new Deck fields; playhead_secs_f32(); reset_playback() updated |
| `performative-core/src/app_state.rs` | head_deck, cue_mix_up, cue_deck, cue_blend fields |
| `performative-core/src/engine.rs` | seek(), cue(), set_loop(), set_head(), pre(), sync() methods; b_query after load; spawn analysis; spawn_loop_monitor(); cue_mix d_load at boot |
| `performative-core/Cargo.toml` | Add serde, serde_json, performative-analysis |
| `performative-tui/src/ui.rs` | BPM, cue markers, loop indicator, sync badge, cue routing in deck panels |
| `performative-tui/src/app.rs` | Call spawn_loop_monitor() after boot |
| `performative-analysis/` (new) | Crate: analyze(), TrackAnalysis, cache |
| `scripts/analyze.py` (new) | Essentia BPM/key/beat detection |
| Root `Cargo.toml` | Add performative-analysis to workspace |

---

## Execution Plan (Feature Agent Delegation)

Each sub-task is a self-contained feature-engineer agent invocation:

1. **T5.1** — Deck model expansion + buffer info (foundation, must land first)
2. **T5.5** — Head command (simplest, quick win)
3. **T5.2 + T5.6** — Seek + Pre (independent, can run in parallel)
4. **T5.3 + T5.4 + T5.7** — Cue + Loop + Analysis (can run in parallel after T5.2)
5. **T5.8** — Sync (needs analysis)
6. **T5.9** — TUI updates (needs all above)

Between each step: `cargo build` + `cargo test` to verify. After T5.9: full E2E test with the acceptance commands.

---

## Verification

```bash
# Build and run tests after each sub-task
cargo build && cargo test

# E2E acceptance (requires scsynth + audio files)
# 1. Boot: cargo run
# 2. load 1 ~/track_a.wav
# 3. load 2 ~/track_b.wav
# 4. play 1
# 5. head 1
# 6. seek 2 bar 16             → deck 2 jumps to bar 16
# 7. cue 2 set A               → cue A saved
# 8. cue 2 A                   → deck 2 jumps back to cue A
# 9. loop 2 4bars              → deck 2 loops
# 10. loop 2 off               → loop released
# 11. pre 2                    → hear deck 2 in headphones (requires BlackHole)
# 12. sync 2                   → deck 2 plays at deck 1's BPM
# 13. 2 | fadein 16bars > 1 | eq lo kill over 4bars > fadeout 1 4bars
#     → compound transition still works with new commands
```
