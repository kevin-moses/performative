use anyhow::{Context, Result};
use performative_osc::{OscClient, Scsynth, messages as msg};
use rosc::OscType;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

use crate::app_state::{ActiveTransition, AppState};
use crate::deck::{BufferInfo, DeckState, LoopState, PendingRamp, RampParam};
pub use performative_parser::{EqBand, RampDuration};
use performative_parser::{Command, CueAction, LoopAction, ParallelStep, PreAction, Script, SeekPosition, Statement, step_max_secs};

const SC_PORT: u16 = 57110;

/// Extract a numeric OSC arg as f32 regardless of the underlying type.
fn osc_to_f32(arg: &OscType) -> Option<f32> {
    match arg {
        OscType::Int(v) => Some(*v as f32),
        OscType::Long(v) => Some(*v as f32),
        OscType::Float(v) => Some(*v),
        OscType::Double(v) => Some(*v as f32),
        _ => None,
    }
}

pub struct AudioEngine {
    pub osc: OscClient,
    state: Arc<Mutex<AppState>>,
}

impl AudioEngine {
    pub async fn new(state: Arc<Mutex<AppState>>) -> Result<Self> {
        let osc = OscClient::new().await.context("failed to create OSC client")?;
        Ok(Self { osc, state })
    }

    /// Boot scsynth, wait for ready, load all SynthDefs. Returns the Scsynth handle
    /// (caller must keep it alive for the session).
    pub async fn boot_scsynth(&self) -> Result<Scsynth> {
        let sc = Scsynth::boot(SC_PORT).context("failed to boot scsynth")?;

        self.osc
            .wait_for_ready(10_000)
            .await
            .context("scsynth did not become ready in 10s")?;

        // scsynth doesn't auto-create the default group — sclang normally does this.
        // Create group 1 (addToHead of root node 0) before anything else.
        self.osc.send(msg::g_new_head(msg::ROOT_GROUP, 0)).await?;

        let synthdefs_dir = synthdefs_dir()?;
        for def in &["deck_player", "deck_eq", "master_mix", "cue_mix"] {
            let path = synthdefs_dir.join(format!("{def}.scsyndef"));
            self.osc
                .send_recv(msg::d_load(path.to_str().unwrap()), "/done", 5_000)
                .await
                .with_context(|| format!("d_load timed out for {def}"))?;
        }

        self.state.lock().await.scsynth_ready = true;
        Ok(sc)
    }

    /// Load a track onto a deck. Updates state immediately; sends b_allocRead fire-and-forget.
    pub async fn load(&self, deck_idx: usize, path: &str) -> Result<()> {
        // Resolve to absolute path so scsynth can find it regardless of cwd.
        let abs = std::fs::canonicalize(path)
            .with_context(|| format!("file not found: {path}"))?;
        let abs_str = abs.to_string_lossy().to_string();

        let track_name = Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Before resetting, free any running synths for this deck so scsynth does
        // not reject the subsequent s_new calls with "duplicate node ID".
        let synths_were_up = {
            let st = self.state.lock().await;
            st.decks[deck_idx].synths_up
        };
        if synths_were_up {
            let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
            let eq_node     = msg::DECK_EQ_BASE     + deck_idx as i32;
            let _ = self.osc.send(msg::n_free(player_node)).await;
            let _ = self.osc.send(msg::n_free(eq_node)).await;
        }

        {
            let mut st = self.state.lock().await;
            let deck = &mut st.decks[deck_idx];
            deck.track_name = Some(track_name.clone());
            deck.track_path = Some(abs_str.clone());
            deck.reset_playback();
        }

        // Transcode to WAV if the format isn't natively supported by libsndfile/scsynth.
        let load_path = transcode_if_needed(&abs_str).await?;

        let buf_num = msg::BUFFER_BASE + deck_idx as i32;
        // Wait for scsynth to confirm the buffer is fully loaded before returning.
        // Without this, a quick `play` command arrives while PlayBuf sees 0 frames
        // and immediately self-frees (doneAction: Done.freeSelf), producing silence.
        self.osc
            .send_recv(
                msg::b_alloc_read(buf_num, load_path.to_str().unwrap()),
                "/done",
                15_000,
            )
            .await
            .context("b_allocRead timed out — is the file readable?")?;

        // Query buffer info. /b_info reply args:
        //   [buf_num: Int, num_frames: Int, num_channels: Int, sample_rate: Float]
        // Types can vary (Int vs Long, Float vs Double) so extract with osc_to_f32.
        if let Ok(info) = self.osc.send_recv(msg::b_query(buf_num), "/b_info", 5_000).await {
            if info.args.len() >= 4 {
                let frames     = osc_to_f32(&info.args[1]);
                let channels   = osc_to_f32(&info.args[2]);
                let sr         = osc_to_f32(&info.args[3]);
                if let (Some(f), Some(c), Some(s)) = (frames, channels, sr) {
                    let buf_info = BufferInfo {
                        num_frames:    f as i32,
                        num_channels:  c as i32,
                        sample_rate:   s,
                    };
                    let duration = buf_info.duration_secs();
                    let mut st = self.state.lock().await;
                    let deck = &mut st.decks[deck_idx];
                    deck.buffer_info   = Some(buf_info);
                    deck.track_duration = if s > 0.0 { Some(duration) } else { None };
                    debug_log(&format!(
                        "LOAD BUFFER deck={} frames={} channels={} sr={:.1} duration={:.4}",
                        deck_idx, f as i32, c as i32, s, duration,
                    ));
                }
            }
        }

        // Restore persisted cue points for this track if available.
        if let Ok(points) = load_cue_points(&abs_str) {
            let mut st = self.state.lock().await;
            st.decks[deck_idx].cue_points = points;
        }

        self.state.lock().await.status_msg =
            format!("Loaded: {track_name} → Deck {}", deck_idx + 1);

        // Spawn background analysis — the deck is immediately playable.
        let state_for_analysis = self.state.clone();
        let path_for_analysis = abs_str.clone();
        tokio::spawn(async move {
            match performative_analysis::analyze(&path_for_analysis).await {
                Ok(analysis) => {
                    let mut st = state_for_analysis.lock().await;
                    st.decks[deck_idx].native_bpm = Some(analysis.bpm);
                    st.decks[deck_idx].key = Some(analysis.key.clone());
                    if st.head_deck == deck_idx {
                        st.bpm = analysis.bpm;
                    }
                    st.status_msg =
                        format!("Analysis: {:.1} BPM, {}", analysis.bpm, analysis.key);
                }
                Err(e) => {
                    let mut st = state_for_analysis.lock().await;
                    st.status_msg = format!("Analysis unavailable: {e}");
                }
            }
        });

        Ok(())
    }

    /// Play (or resume) a deck.
    pub async fn play(&self, deck_idx: usize) -> Result<()> {
        let (synths_up, master_up) = {
            let st = self.state.lock().await;
            (st.decks[deck_idx].synths_up, st.master_up)
        };

        if !synths_up {
            self.create_deck_synths(deck_idx, master_up).await?;
            // scsynth processes messages per block (~1.5ms at 44100Hz/64 frames).
            // Without this gap, a following n_set (e.g. from `> fadein`) arrives in
            // the same block as the s_new, so VarLag initialises from the n_set's
            // gain=1.0 instead of the s_new's gain=0.0 — producing instant full volume.
            sleep(Duration::from_millis(20)).await;
            {
                let mut st = self.state.lock().await;
                st.decks[deck_idx].synths_up = true;
                st.master_up = true;
            }
        } else {
            // Resume a paused node.
            self.osc
                .send(msg::n_run(msg::DECK_PLAYER_BASE + deck_idx as i32, true))
                .await?;
        }

        {
            let mut st = self.state.lock().await;
            let deck = &mut st.decks[deck_idx];
            deck.state = DeckState::Playing;
            deck.play_start = Some(Instant::now());
            st.status_msg = format!("Playing Deck {}", deck_idx + 1);
        }

        // Apply any ramps that were queued while the deck was paused/stopped.
        self.drain_pending_ramps(deck_idx).await?;

        Ok(())
    }

    async fn drain_pending_ramps(&self, deck_idx: usize) -> Result<()> {
        let pending = {
            let mut st = self.state.lock().await;
            std::mem::take(&mut st.decks[deck_idx].pending_ramps)
        };
        for pr in pending {
            let (node_id, val_key, lag_key) = ramp_param_keys(deck_idx, pr.param);
            let from = get_deck_param(&self.state.lock().await.decks[deck_idx], pr.param);
            self.osc
                .send(msg::n_set(node_id, &[
                    (lag_key, pr.duration_secs),
                    (val_key, pr.target),
                ]))
                .await?;
            spawn_ramp_tasks(self.state.clone(), pr.param, deck_idx, from, pr.target, pr.duration_secs);
        }
        Ok(())
    }

    async fn queue_ramp(&self, deck_idx: usize, param: RampParam, target: f32, duration_secs: f32, what: &str) {
        let mut st = self.state.lock().await;
        let deck = &mut st.decks[deck_idx];
        deck.pending_ramps.retain(|pr| pr.param != param);
        deck.pending_ramps.push(PendingRamp { param, target, duration_secs });
        st.status_msg = format!("Deck {} {} → {:.2} over {:.1}s (queued)", deck_idx + 1, what, target, duration_secs);
    }

    /// Pause a deck.
    pub async fn pause(&self, deck_idx: usize) -> Result<()> {
        self.osc
            .send(msg::n_run(msg::DECK_PLAYER_BASE + deck_idx as i32, false))
            .await?;

        {
            let mut st = self.state.lock().await;
            let deck = &mut st.decks[deck_idx];
            if let Some(started) = deck.play_start.take() {
                deck.elapsed_before += started.elapsed();
            }
            deck.state = DeckState::Paused;
            st.status_msg = format!("Paused Deck {}", deck_idx + 1);
        }
        Ok(())
    }

    /// Jump the playhead of a deck to the given position.
    ///
    /// Kills the current player synth and recreates it at the target frame so
    /// scsynth's PlayBuf starts from the new position.  All other deck state
    /// (gain, EQ, rate, loop) is preserved.
    ///
    /// Steps:
    ///   1. Resolve `position` to an absolute `target_secs` value.
    ///   2. Clamp to `0.0..buffer_duration`.
    ///   3. Convert to frame index.
    ///   4. Free the old player synth (`/n_free`).
    ///   5. Create a new player synth at the target frame (`/s_new` with `start`
    ///      param, added before the EQ node).
    ///   6. If the deck was NOT playing, immediately pause the new synth.
    ///   7. Update Rust-side elapsed tracking so the TUI stays in sync.
    pub async fn seek(&self, deck_idx: usize, position: SeekPosition) -> Result<()> {
        // Capture debug representation before the match consumes `position`.
        let dbg_position = format!("{:?}", position);

        // ── 1. Resolve position to seconds ────────────────────────────────────
        let target_secs: f32 = {
            let st = self.state.lock().await;
            let deck = &st.decks[deck_idx];
            let bpm = deck.native_bpm.unwrap_or(st.bpm);
            let current_secs = deck.playback_elapsed.max(0.0);

            match position {
                SeekPosition::Seconds(s) => s,
                SeekPosition::Bar(b) => {
                    // Bar is 1-indexed: bar 1 = start of track.
                    // Each bar = 4 beats at `bpm` BPM.
                    // Duration of one bar in seconds = 60.0 / bpm * 4.0
                    (b - 1.0) * 60.0 * 4.0 / bpm
                }
                SeekPosition::RelativeBars(b) => {
                    let secs_per_bar = 60.0 * 4.0 / bpm;
                    current_secs + b * secs_per_bar
                }
                SeekPosition::RelativeSeconds(s) => current_secs + s,
                SeekPosition::CuePoint(label) => {
                    match deck.cue_points.get(&label) {
                        Some(&secs) => secs,
                        None => {
                            return Err(anyhow::anyhow!(
                                "Cue point {} not set on deck {}",
                                label,
                                deck_idx + 1
                            ));
                        }
                    }
                }
            }
        };

        debug_log(&format!(
            "SEEK deck={} position={} target_secs={:.4}",
            deck_idx, dbg_position, target_secs,
        ));

        // ── 2. Clamp to buffer duration ────────────────────────────────────────
        let (target_secs, is_playing, gain, rate, buf_num) = {
            let st = self.state.lock().await;
            let deck = &st.decks[deck_idx];
            let duration = deck
                .buffer_info
                .as_ref()
                .map(|bi| bi.duration_secs())
                .filter(|d| *d > 0.0)
                .unwrap_or(f32::MAX);
            let clamped = target_secs.clamp(0.0, duration);
            debug_log(&format!(
                "SEEK CLAMP deck={} duration={:.4} clamped={:.4} buf_info={:?}",
                deck_idx, duration, clamped,
                deck.buffer_info.as_ref().map(|bi| (bi.num_frames, bi.num_channels, bi.sample_rate)),
            ));
            let is_playing = deck.state == DeckState::Playing;
            let buf_num = (msg::BUFFER_BASE + deck_idx as i32) as f32;
            (clamped, is_playing, deck.gain, deck.rate, buf_num)
        };

        // ── 3. Convert to frame index ──────────────────────────────────────────
        let target_frames: i32 = {
            let st = self.state.lock().await;
            let deck = &st.decks[deck_idx];
            match &deck.buffer_info {
                Some(bi) if bi.sample_rate > 0.0 => bi.secs_to_frames(target_secs),
                _ => (target_secs * 44100.0) as i32,
            }
        };

        let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
        let eq_node     = msg::DECK_EQ_BASE     + deck_idx as i32;
        let out_bus     = (msg::DECK_BUS_BASE   + deck_idx as i32 * 2) as f32;

        debug_log(&format!(
            "SEEK EXEC deck={} target_secs={:.4} target_frames={} player_node={} eq_node={} is_playing={} rate={:.4}",
            deck_idx, target_secs, target_frames, player_node, eq_node, is_playing, rate,
        ));

        // ── 4. Free the old player synth ──────────────────────────────────────
        self.osc.send(msg::n_free(player_node)).await?;

        // ── 5. Create a new player synth at the target position ──────────────
        // addBefore (action=2) the EQ node keeps the graph order: player → eq → master.
        self.osc
            .send(msg::s_new(
                "deck_player",
                player_node,
                2,        // addBefore
                eq_node,  // target: EQ node
                &[
                    ("buf",      buf_num),
                    ("rate",     rate),
                    ("rate_lag", 0.0),
                    ("gain",     gain),
                    ("out_bus",  out_bus),
                    ("loop_",    0.0),
                    ("pos",      target_frames as f32),
                ],
            ))
            .await?;

        // ── 6. If not playing, immediately pause the new synth ────────────────
        if !is_playing {
            self.osc
                .send(msg::n_run(player_node, false))
                .await?;
        }

        // ── 7. Update Rust-side elapsed tracking ──────────────────────────────
        let normalised = {
            let mut st = self.state.lock().await;
            let deck = &mut st.decks[deck_idx];
            // Normalise elapsed by rate so that playback_elapsed * rate == target_secs.
            let normalised = if deck.rate > 0.0 {
                target_secs / deck.rate
            } else {
                target_secs
            };
            deck.playback_elapsed = normalised;
            // Reset wall-clock play_start to now when playing so elapsed stays accurate.
            if is_playing {
                deck.play_start = Some(Instant::now());
            }
            st.status_msg = format!("Deck {} seeked to {:.1}s", deck_idx + 1, target_secs);
            normalised
        };
        debug_log(&format!(
            "SEEK DONE deck={} new_elapsed={:.4}",
            deck_idx, normalised,
        ));

        Ok(())
    }

    /// Set the BPM/scheduling reference deck.
    ///
    /// Updates `head_deck` and, if the new head deck has a known `native_bpm`,
    /// adopts that as the session BPM.
    pub async fn set_head(&self, deck_idx: usize) {
        let mut st = self.state.lock().await;
        st.head_deck = deck_idx;
        if let Some(bpm) = st.decks[deck_idx].native_bpm {
            st.bpm = bpm;
        }
        st.status_msg = format!("Deck {} is now head", deck_idx + 1);
    }

    /// Route a deck to the cue (headphone) bus, stop cue routing, or adjust the blend.
    ///
    /// - `PreAction::Deck(idx)`: create the `cue_mix` synth if it does not exist, or
    ///   redirect it to the new deck's bus. Sets `cue_active` on the target deck and
    ///   clears it from any previous cue deck.
    /// - `PreAction::Off`: free the `cue_mix` synth node and clear all cue state.
    /// - `PreAction::Blend(v)`: adjust the dry/wet blend on the live `cue_mix` node.
    pub async fn pre(&self, action: PreAction) -> Result<()> {
        match action {
            PreAction::Deck(deck_idx) => {
                let (cue_mix_up, prev_cue_deck, cue_bus) = {
                    let st = self.state.lock().await;
                    let cue_bus = (msg::DECK_BUS_BASE + deck_idx as i32 * 2) as f32;
                    (st.cue_mix_up, st.cue_deck, cue_bus)
                };

                if !cue_mix_up {
                    // Create the cue_mix synth for the first time.
                    self.osc
                        .send(msg::s_new(
                            "cue_mix",
                            msg::CUE_MIX_NODE,
                            1, // addToTail of the default group
                            msg::ROOT_GROUP,
                            &[("cue_bus", cue_bus)],
                        ))
                        .await?;
                } else {
                    // Redirect the existing cue_mix synth to the new deck's bus.
                    self.osc
                        .send(msg::n_set(msg::CUE_MIX_NODE, &[("cue_bus", cue_bus)]))
                        .await?;
                }

                {
                    let mut st = self.state.lock().await;
                    // Clear cue_active from the previous cue deck.
                    if let Some(prev) = prev_cue_deck {
                        st.decks[prev].cue_active = false;
                    }
                    st.decks[deck_idx].cue_active = true;
                    st.cue_deck = Some(deck_idx);
                    st.cue_mix_up = true;
                    st.status_msg = format!("Pre: Deck {} -> cue bus", deck_idx + 1);
                }
            }

            PreAction::Off => {
                self.osc.send(msg::n_free(msg::CUE_MIX_NODE)).await?;

                let mut st = self.state.lock().await;
                for deck in st.decks.iter_mut() {
                    deck.cue_active = false;
                }
                st.cue_mix_up = false;
                st.cue_deck = None;
                st.status_msg = "Pre: cue off".into();
            }

            PreAction::Blend(v) => {
                self.osc
                    .send(msg::n_set(msg::CUE_MIX_NODE, &[("blend", v)]))
                    .await?;

                let mut st = self.state.lock().await;
                st.cue_blend = v;
                st.status_msg = format!("Pre blend: {:.2}", v);
            }
        }
        Ok(())
    }

    /// Set a hot cue point at the current playhead, or jump to an existing one.
    ///
    /// - `CueAction::Set(label)`: records the current playhead position under `label`
    ///   in the deck's `cue_points` map, then persists all cue points to
    ///   `~/.performative/cache/<track_hash>/cues.json`.
    ///
    /// - `CueAction::Jump(label)`: looks up `label` in `deck.cue_points`; if found,
    ///   delegates to `seek()` with `SeekPosition::Seconds(pos)`. If not found, sets
    ///   an error status message and returns Ok (not a hard error, stays operational).
    pub async fn cue(&self, deck_idx: usize, action: CueAction) -> Result<()> {
        match action {
            CueAction::Set(label) => {
                // Snapshot position and track path before mutating state.
                let (pos, track_path, dbg_elapsed, dbg_rate, dbg_state) = {
                    let st = self.state.lock().await;
                    let deck = &st.decks[deck_idx];
                    (
                        deck.playhead_secs_f32(),
                        deck.track_path.clone(),
                        deck.playback_elapsed,
                        deck.rate,
                        format!("{:?}", deck.state),
                    )
                };
                debug_log(&format!(
                    "CUE SET deck={} label={} elapsed={:.4} rate={:.4} pos={:.4} state={}",
                    deck_idx, label, dbg_elapsed, dbg_rate, pos, dbg_state,
                ));

                {
                    let mut st = self.state.lock().await;
                    st.decks[deck_idx].cue_points.insert(label, pos);
                    st.status_msg = format!("Cue {} set at {:.1}s", label, pos);
                }

                // Persist cue points to disk if we know the track path.
                if let Some(path) = track_path {
                    let cue_points = self.state.lock().await.decks[deck_idx].cue_points.clone();
                    if let Err(e) = persist_cue_points(&path, &cue_points) {
                        // Non-fatal: warn in status but don't propagate.
                        self.state.lock().await.status_msg =
                            format!("Cue {} set at {:.1}s (save failed: {e})", label, pos);
                    }
                }

                Ok(())
            }

            CueAction::Jump(label) => {
                let pos = {
                    let st = self.state.lock().await;
                    st.decks[deck_idx].cue_points.get(&label).copied()
                };
                debug_log(&format!(
                    "CUE JUMP deck={} label={} stored_pos={:?}",
                    deck_idx, label, pos,
                ));

                match pos {
                    Some(secs) => self.seek(deck_idx, SeekPosition::Seconds(secs)).await,
                    None => {
                        self.state.lock().await.status_msg =
                            format!("Cue point {} not set", label);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Set deck gain instantly or with a timed ramp.
    pub async fn set_gain(
        &self,
        deck_idx: usize,
        target: f32,
        ramp: Option<RampDuration>,
    ) -> Result<()> {
        let duration_secs = ramp_to_secs(&ramp, &self.state).await;
        let is_playing = self.state.lock().await.decks[deck_idx].state == DeckState::Playing;

        if duration_secs > 0.0 && !is_playing {
            self.queue_ramp(deck_idx, RampParam::Gain, target, duration_secs, "gain").await;
            return Ok(());
        }

        let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
        self.osc
            .send(msg::n_set(player_node, &[
                ("gain_lag", duration_secs),
                ("gain",     target),
            ]))
            .await?;

        let from = {
            let mut st = self.state.lock().await;
            let from = st.decks[deck_idx].gain;
            if duration_secs == 0.0 {
                st.decks[deck_idx].gain = target;
            }
            st.status_msg = format!(
                "Deck {} gain → {:.2}{}",
                deck_idx + 1, target, ramp_label(&ramp, duration_secs),
            );
            from
        };

        if duration_secs > 0.0 {
            spawn_ramp_tasks(self.state.clone(), RampParam::Gain, deck_idx, from, target, duration_secs);
        }
        Ok(())
    }

    /// Set an EQ band instantly or with a timed ramp.
    pub async fn set_eq(
        &self,
        deck_idx: usize,
        band: EqBand,
        target: f32,
        ramp: Option<RampDuration>,
    ) -> Result<()> {
        let duration_secs = ramp_to_secs(&ramp, &self.state).await;
        let is_playing = self.state.lock().await.decks[deck_idx].state == DeckState::Playing;
        let rp = eq_band_to_ramp_param(band);

        if duration_secs > 0.0 && !is_playing {
            let what = format!("EQ {}", eq_band_name(band));
            self.queue_ramp(deck_idx, rp, target, duration_secs, &what).await;
            return Ok(());
        }

        let eq_node = msg::DECK_EQ_BASE + deck_idx as i32;
        let (val_key, lag_key) = ramp_param_val_lag(rp);
        self.osc
            .send(msg::n_set(eq_node, &[
                (lag_key, duration_secs),
                (val_key, target),
            ]))
            .await?;

        let from = {
            let mut st = self.state.lock().await;
            let from = get_deck_param(&st.decks[deck_idx], rp);
            if duration_secs == 0.0 {
                set_deck_param(&mut st.decks[deck_idx], rp, target);
            }
            st.status_msg = format!(
                "Deck {} EQ {} → {:.2}{}",
                deck_idx + 1, eq_band_name(band), target,
                ramp_label(&ramp, duration_secs),
            );
            from
        };

        if duration_secs > 0.0 {
            spawn_ramp_tasks(self.state.clone(), rp, deck_idx, from, target, duration_secs);
        }
        Ok(())
    }

    // ── Composition execution ────────────────────────────────────────────────

    /// Execute a parsed `Script`. Each `;`-separated statement is spawned as an independent
    /// background task. Returns immediately after spawning — does not wait for completion.
    pub async fn execute_script(self: Arc<Self>, script: Script, label: String) -> Result<()> {
        for stmt in script.statements {
            let engine = Arc::clone(&self);
            let label = label.clone();
            tokio::spawn(async move {
                if let Err(e) = engine.execute_statement(stmt, label).await {
                    engine.state.lock().await.status_msg = format!("error: {e}");
                }
            });
        }
        Ok(())
    }

    /// Execute one statement: run each step in sequence, sleeping between steps for
    /// the duration of the previous step's longest ramp.
    async fn execute_statement(&self, stmt: Statement, label: String) -> Result<()> {
        let bpm = self.state.lock().await.bpm;
        let mut prev_secs = 0.0f32;
        let mut set_transition = false;

        for (i, step) in stmt.steps.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_secs_f32(prev_secs)).await;
            }

            let step_secs = step_max_secs(step, bpm);
            if step_secs > 0.0 {
                let mut st = self.state.lock().await;
                st.active_transition = Some(ActiveTransition {
                    label: label.clone(),
                    start: Instant::now(),
                    total_secs: step_secs,
                });
                set_transition = true;
            }

            self.execute_parallel_step(step).await?;
            prev_secs = step_secs;
        }

        if set_transition {
            self.state.lock().await.active_transition = None;
        }
        Ok(())
    }

    /// Execute all commands in a parallel step (sequentially — OSC messages are fire-and-forget).
    async fn execute_parallel_step(&self, step: &ParallelStep) -> Result<()> {
        for cmd in &step.commands {
            self.execute_command(cmd.clone()).await?;
        }
        Ok(())
    }

    /// Dispatch a single `Command` to the appropriate engine method.
    pub async fn execute_command(&self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Load { deck, path }             => self.load(deck, &path).await,
            Command::Play { deck }                   => self.play(deck).await,
            Command::Pause { deck }                  => self.pause(deck).await,
            Command::Gain { deck, target, ramp }     => self.set_gain(deck, target, ramp).await,
            Command::Eq { deck, band, target, ramp } => self.set_eq(deck, band, target, ramp).await,
            Command::Jog { deck } => {
                let mut st = self.state.lock().await;
                st.jog_deck = Some(deck);
                st.status_msg = format!("Jog: Deck {}", deck + 1);
                Ok(())
            }
            Command::Head { deck } => {
                self.set_head(deck).await;
                Ok(())
            }
            Command::Seek { deck, position } => self.seek(deck, position).await,
            Command::Pre { action } => self.pre(action).await,
            Command::Loop { deck, action } => {
                self.set_loop(deck, action).await;
                Ok(())
            }
            Command::Cue { deck, action } => self.cue(deck, action).await,
            Command::Quit => {
                self.state.lock().await.status_msg = "Type Esc or ctrl-c to quit.".into();
                Ok(())
            }
        }
    }

    /// Set, modify, or clear a loop region on a deck.
    ///
    /// purpose: handle all `loop` sub-commands for a given deck.
    ///   - Set(duration): capture current playhead as in_secs, compute out_secs from
    ///     duration in seconds (using deck native_bpm or session BPM), clamp to buffer,
    ///     and store LoopState.
    ///   - Off: clear the active loop.
    ///   - Halve: halve the loop length (out_secs moves toward in_secs).
    ///   - Double: double the loop length (out_secs moves away from in_secs).
    /// @param deck_idx: (usize) 0-based deck index
    /// @param action: (LoopAction) the loop sub-action to perform
    pub async fn set_loop(&self, deck_idx: usize, action: LoopAction) {
        let mut st = self.state.lock().await;
        let bpm = st.decks[deck_idx].native_bpm.unwrap_or(st.bpm);

        match action {
            LoopAction::Set(duration) => {
                let in_secs = st.decks[deck_idx].playhead_secs_f32();
                let duration_secs = duration.to_secs(bpm);
                let mut out_secs = in_secs + duration_secs;

                // Clamp out_secs to buffer length if buffer info is available.
                if let Some(ref bi) = st.decks[deck_idx].buffer_info {
                    let dur = bi.duration_secs();
                    if dur > 0.0 {
                        out_secs = out_secs.min(dur);
                    }
                }

                // Compute length in bars: duration_secs / (4 * 60 / bpm)
                let secs_per_bar = 4.0 * 60.0 / bpm;
                let length_bars = if secs_per_bar > 0.0 {
                    duration_secs / secs_per_bar
                } else {
                    0.0
                };

                st.decks[deck_idx].loop_state = Some(LoopState {
                    in_secs,
                    out_secs,
                    length_bars,
                });
                st.status_msg = format!(
                    "Deck {} loop: {:.1}s – {:.1}s ({:.2} bars)",
                    deck_idx + 1, in_secs, out_secs, length_bars
                );
            }

            LoopAction::Off => {
                st.decks[deck_idx].loop_state = None;
                st.status_msg = format!("Deck {} loop off", deck_idx + 1);
            }

            LoopAction::Halve => {
                let deck = &mut st.decks[deck_idx];
                if let Some(ref mut ls) = deck.loop_state {
                    let half_len = (ls.out_secs - ls.in_secs) / 2.0;
                    ls.out_secs = ls.in_secs + half_len;
                    ls.length_bars /= 2.0;
                    let msg = format!(
                        "Deck {} loop halved: {:.1}s – {:.1}s ({:.2} bars)",
                        deck_idx + 1, ls.in_secs, ls.out_secs, ls.length_bars
                    );
                    st.status_msg = msg;
                } else {
                    st.status_msg = format!("Deck {}: no active loop to halve", deck_idx + 1);
                }
            }

            LoopAction::Double => {
                let deck = &mut st.decks[deck_idx];
                if let Some(ref mut ls) = deck.loop_state {
                    let new_len = (ls.out_secs - ls.in_secs) * 2.0;
                    ls.out_secs = ls.in_secs + new_len;
                    ls.length_bars *= 2.0;
                    let msg = format!(
                        "Deck {} loop doubled: {:.1}s – {:.1}s ({:.2} bars)",
                        deck_idx + 1, ls.in_secs, ls.out_secs, ls.length_bars
                    );
                    st.status_msg = msg;
                } else {
                    st.status_msg = format!("Deck {}: no active loop to double", deck_idx + 1);
                }
            }
        }
    }

    /// Spawn the background loop monitor task.
    ///
    /// purpose: poll each deck at ~30ms intervals and, when a deck is playing with an
    ///          active loop whose playhead has reached or passed out_secs, seek it back
    ///          to in_secs. Must be called after scsynth is booted (since seek sends
    ///          OSC messages to scsynth).
    /// @param self: must be an Arc<AudioEngine> so the spawned task can hold a clone
    pub fn spawn_loop_monitor(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(30));
            loop {
                ticker.tick().await;
                for deck_idx in 0..2 {
                    let should_seek = {
                        let st = engine.state.lock().await;
                        let deck = &st.decks[deck_idx];
                        if deck.state == DeckState::Playing {
                            if let Some(ref ls) = deck.loop_state {
                                deck.playhead_secs_f32() >= ls.out_secs
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    };

                    if should_seek {
                        let in_secs = {
                            let st = engine.state.lock().await;
                            st.decks[deck_idx]
                                .loop_state
                                .as_ref()
                                .map(|ls| ls.in_secs)
                        };
                        if let Some(secs) = in_secs {
                            let _ = engine.seek(deck_idx, SeekPosition::Seconds(secs)).await;
                        }
                    }
                }
            }
        });
    }

    async fn create_deck_synths(&self, deck_idx: usize, master_up: bool) -> Result<()> {
        let buf_num = (msg::BUFFER_BASE + deck_idx as i32) as f32;
        let out_bus = (msg::DECK_BUS_BASE + deck_idx as i32 * 2) as f32;
        let player_node = msg::DECK_PLAYER_BASE + deck_idx as i32;
        let eq_node = msg::DECK_EQ_BASE + deck_idx as i32;

        // Snapshot current gain values so synths start at the right level.
        let (gain, lo_gain, mid_gain, hi_gain) = {
            let st = self.state.lock().await;
            let d = &st.decks[deck_idx];
            (d.gain, d.lo_gain, d.mid_gain, d.hi_gain)
        };

        // If master_mix already exists, insert this deck's synths *before* it so
        // the audio graph order is: player → eq → master_mix.
        // Without this, addToTail would place them after master_mix, causing silence.
        let (add_action, target) = if master_up {
            (2, msg::MASTER_MIX_NODE) // addBefore master_mix
        } else {
            (1, msg::ROOT_GROUP)      // addToTail of root group (first deck)
        };

        // player → private bus
        self.osc
            .send(msg::s_new(
                "deck_player",
                player_node,
                add_action,
                target,
                &[
                    ("buf", buf_num),
                    ("rate", 1.0),
                    ("rate_lag", 0.0),  // no lag by default; jog sets this dynamically
                    ("gain", gain),
                    ("out_bus", out_bus),
                    ("loop_", 0.0),
                    ("pos", 0.0),
                ],
            ))
            .await?;

        // eq → in-place on same bus, also before master_mix
        self.osc
            .send(msg::s_new(
                "deck_eq",
                eq_node,
                add_action,
                target,
                &[
                    ("in_bus", out_bus),
                    ("out_bus", out_bus),
                    ("lo_gain", lo_gain),
                    ("mid_gain", mid_gain),
                    ("hi_gain", hi_gain),
                ],
            ))
            .await?;

        // master_mix — create only once for the session
        if !master_up {
            self.osc
                .send(msg::s_new(
                    "master_mix",
                    msg::MASTER_MIX_NODE,
                    1, // addToTail
                    msg::ROOT_GROUP,
                    &[
                        ("in_bus1", msg::DECK_BUS_BASE as f32),
                        ("in_bus2", (msg::DECK_BUS_BASE + 2) as f32),
                    ],
                ))
                .await?;
        }

        Ok(())
    }
}

// ── Ramp helpers ─────────────────────────────────────────────────────────────

async fn ramp_to_secs(ramp: &Option<RampDuration>, state: &Arc<Mutex<AppState>>) -> f32 {
    match ramp {
        Some(r) => r.to_secs(state.lock().await.bpm),
        None    => 0.0,
    }
}

fn ramp_label(ramp: &Option<RampDuration>, secs: f32) -> String {
    match ramp {
        Some(_) => format!(" over {secs:.1}s"),
        None    => String::new(),
    }
}

/// Interpolates the Rust-side deck param at ~30 fps so the TUI shows a live ramp.
///
/// No lag reset is sent — every `n_set` already includes the lag value explicitly,
/// so the next command (instant or ramp) always sets the correct lag inline.
fn spawn_ramp_tasks(
    state: Arc<Mutex<AppState>>,
    param: RampParam,
    deck_idx: usize,
    from: f32,
    to: f32,
    secs: f32,
) {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        loop {
            sleep(Duration::from_millis(16)).await;
            let elapsed = start.elapsed().as_secs_f32();
            let t = (elapsed / secs).min(1.0);
            let value = from + (to - from) * t;
            set_deck_param(&mut state.lock().await.decks[deck_idx], param, value);
            if t >= 1.0 { break; }
        }
    });
}

fn get_deck_param(deck: &crate::deck::Deck, param: RampParam) -> f32 {
    match param {
        RampParam::Gain => deck.gain,
        RampParam::Lo   => deck.lo_gain,
        RampParam::Mid  => deck.mid_gain,
        RampParam::Hi   => deck.hi_gain,
    }
}

fn set_deck_param(deck: &mut crate::deck::Deck, param: RampParam, value: f32) {
    match param {
        RampParam::Gain => deck.gain     = value,
        RampParam::Lo   => deck.lo_gain  = value,
        RampParam::Mid  => deck.mid_gain = value,
        RampParam::Hi   => deck.hi_gain  = value,
    }
}

fn eq_band_to_ramp_param(band: EqBand) -> RampParam {
    match band {
        EqBand::Lo  => RampParam::Lo,
        EqBand::Mid => RampParam::Mid,
        EqBand::Hi  => RampParam::Hi,
    }
}

fn eq_band_name(band: EqBand) -> &'static str {
    match band {
        EqBand::Lo => "lo", EqBand::Mid => "mid", EqBand::Hi => "hi",
    }
}

/// Returns `(node_id, val_key, lag_key)` for a given `RampParam`.
fn ramp_param_keys(deck_idx: usize, param: RampParam) -> (i32, &'static str, &'static str) {
    let node_id = match param {
        RampParam::Gain => msg::DECK_PLAYER_BASE + deck_idx as i32,
        _               => msg::DECK_EQ_BASE     + deck_idx as i32,
    };
    let (val_key, lag_key) = ramp_param_val_lag(param);
    (node_id, val_key, lag_key)
}

fn ramp_param_val_lag(param: RampParam) -> (&'static str, &'static str) {
    match param {
        RampParam::Gain => ("gain",     "gain_lag"),
        RampParam::Lo   => ("lo_gain",  "lo_lag"),
        RampParam::Mid  => ("mid_gain", "mid_lag"),
        RampParam::Hi   => ("hi_gain",  "hi_lag"),
    }
}

/// Extensions natively readable by libsndfile (and therefore scsynth).
const LIBSNDFILE_EXTS: &[&str] = &[
    "wav", "aiff", "aif", "au", "snd", "flac", "ogg", "oga", "caf", "w64", "rf64",
];

/// If `path` is in a format scsynth can't read directly (e.g. MP3, AAC, M4A),
/// decode it to a temp WAV using ffmpeg and return the temp path.
/// Otherwise return the original path unchanged.
async fn transcode_if_needed(path: &str) -> Result<PathBuf> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if LIBSNDFILE_EXTS.contains(&ext.as_str()) {
        return Ok(PathBuf::from(path));
    }

    // Need ffmpeg — check it's available.
    let ffmpeg = which_ffmpeg().context(
        "ffmpeg not found. Install it: brew install ffmpeg\n\
         ffmpeg is required to load MP3/AAC/M4A/OPUS files.",
    )?;

    // Write to ~/.performative/cache/transcode/<hash>.wav
    let cache_dir = dirs_cache_transcode()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create transcode cache dir: {}", cache_dir.display()))?;

    // Use a hash of the source path as the cache key (simple, not cryptographic).
    let hash = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        path.hash(&mut h);
        h.finish()
    };
    let out_path = cache_dir.join(format!("{hash:016x}.wav"));

    if !out_path.exists() {
        // ffmpeg -i <input> -ar 44100 -ac 2 -f wav <output>
        let status = tokio::process::Command::new(&ffmpeg)
            .args([
                "-y",           // overwrite output if exists
                "-i", path,
                "-ar", "44100", // sample rate
                "-ac", "2",     // stereo
                "-f", "wav",
                out_path.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("failed to run ffmpeg")?;

        if !status.success() {
            anyhow::bail!("ffmpeg failed to transcode '{path}'");
        }
    }

    Ok(out_path)
}

fn which_ffmpeg() -> Option<PathBuf> {
    for candidate in &["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        if Path::new(candidate).exists() {
            return Some(PathBuf::from(candidate));
        }
    }
    // Fall back to PATH
    std::process::Command::new("which")
        .arg("ffmpeg")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(PathBuf::from(s)) }
        })
}

fn dirs_cache_transcode() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".performative").join("cache").join("transcode"))
}

/// Return the cache directory for a specific track, keyed by a hash of its path.
///
/// purpose: provide a stable per-track directory under `~/.performative/cache/<hash>/`
///          where persistent metadata (cue points, waveform data, etc.) can be stored.
/// @param track_path: (str) absolute path to the audio file
/// @return: `PathBuf` to `~/.performative/cache/<16-hex-hash>/`
fn dirs_cache_for_track(track_path: &str) -> Result<PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    track_path.hash(&mut h);
    let hash = h.finish();
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home)
        .join(".performative")
        .join("cache")
        .join(format!("{hash:016x}")))
}

/// Write the deck's cue points to `~/.performative/cache/<hash>/cues.json`.
///
/// purpose: persist hot cue points so they survive across sessions.
/// @param track_path: (str) absolute path to the audio file (used to derive cache dir)
/// @param cue_points: reference to the map of label → seconds
/// @return: Ok(()) on success, or an error if the file cannot be written
fn persist_cue_points(
    track_path: &str,
    cue_points: &std::collections::HashMap<char, f32>,
) -> Result<()> {
    let dir = dirs_cache_for_track(track_path)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create cue cache dir: {}", dir.display()))?;

    // Serialize as a JSON object with string keys: {"A": 32.5, "B": 96.0}
    let map: std::collections::HashMap<String, f32> = cue_points
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();

    let json = serde_json::to_string_pretty(&map)
        .context("failed to serialize cue points")?;

    std::fs::write(dir.join("cues.json"), json)
        .context("failed to write cues.json")?;

    Ok(())
}

/// Append a timestamped diagnostic line to `~/.performative/debug.log`.
///
/// purpose: low-level debug logging for tracing cue/seek code paths without
///          attaching a full tracing subscriber. Failures are silently ignored
///          so this never affects runtime behaviour.
/// @param msg: (&str) the message to append
fn debug_log(msg: &str) {
    use std::io::Write;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(home)
        .join(".performative")
        .join("debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "[{:.3}] {}", now, msg);
    }
}

/// Read persisted cue points from `~/.performative/cache/<hash>/cues.json`.
///
/// purpose: restore hot cue points from a previous session on track load.
/// @param track_path: (str) absolute path to the audio file (used to derive cache dir)
/// @return: populated cue point map, or an empty map if no cues.json exists or it cannot be read
fn load_cue_points(track_path: &str) -> Result<std::collections::HashMap<char, f32>> {
    let path = dirs_cache_for_track(track_path)?.join("cues.json");
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let raw: std::collections::HashMap<String, f32> = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mut result = std::collections::HashMap::new();
    for (k, v) in raw {
        if let Some(ch) = k.chars().next() {
            if ('A'..='D').contains(&ch) {
                result.insert(ch, v);
            }
        }
    }
    Ok(result)
}

fn synthdefs_dir() -> Result<std::path::PathBuf> {
    if let Ok(mut p) = std::env::current_exe() {
        p.pop();
        let d = p.join("synthdefs");
        if d.exists() {
            return Ok(d);
        }
    }
    if let Ok(p) = std::env::var("PERFORMATIVE_SYNTHDEFS") {
        let d = std::path::PathBuf::from(p);
        if d.exists() {
            return Ok(d);
        }
    }
    let mut cwd = std::env::current_dir()?;
    loop {
        let d = cwd.join("synthdefs");
        if d.exists() {
            return Ok(d);
        }
        if !cwd.pop() {
            break;
        }
    }
    anyhow::bail!("synthdefs dir not found — set PERFORMATIVE_SYNTHDEFS or run from project root")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;

    /// Build a minimal `AudioEngine` backed by a real `AppState` but without a
    /// live OSC connection. The `osc` field is never exercised by `set_head`, so
    /// this is safe for unit tests.
    async fn make_engine() -> AudioEngine {
        let state = Arc::new(Mutex::new(AppState::new()));
        // We cannot call AudioEngine::new() without a reachable scsynth, so we
        // construct it directly using the public fields.
        let osc = OscClient::new()
            .await
            .expect("OscClient::new() must not require a live server");
        AudioEngine { osc, state }
    }

    #[tokio::test]
    async fn set_head_updates_head_deck() {
        let engine = make_engine().await;
        engine.set_head(1).await;
        let st = engine.state.lock().await;
        assert_eq!(st.head_deck, 1);
    }

    #[tokio::test]
    async fn set_head_adopts_native_bpm_when_present() {
        let engine = make_engine().await;
        {
            let mut st = engine.state.lock().await;
            st.decks[1].native_bpm = Some(140.0);
        }
        engine.set_head(1).await;
        let st = engine.state.lock().await;
        assert!((st.bpm - 140.0).abs() < 1e-6, "expected bpm=140.0, got {}", st.bpm);
    }

    #[tokio::test]
    async fn set_head_does_not_change_bpm_when_native_bpm_absent() {
        let engine = make_engine().await;
        // Deck 0 has no native_bpm — global bpm stays at 120.0.
        engine.set_head(0).await;
        let st = engine.state.lock().await;
        assert!((st.bpm - 120.0).abs() < 1e-6, "expected bpm=120.0, got {}", st.bpm);
    }

    #[tokio::test]
    async fn set_head_sets_status_message() {
        let engine = make_engine().await;
        engine.set_head(0).await;
        let st = engine.state.lock().await;
        assert_eq!(st.status_msg, "Deck 1 is now head");
    }

    #[tokio::test]
    async fn set_head_deck2_sets_status_message() {
        let engine = make_engine().await;
        engine.set_head(1).await;
        let st = engine.state.lock().await;
        assert_eq!(st.status_msg, "Deck 2 is now head");
    }
}
