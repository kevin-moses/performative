use anyhow::{Context, Result};
use performative_osc::{OscClient, Scsynth, messages as msg};
use rosc::OscType;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

use crate::app_state::{ActiveTransition, AppState};
use crate::deck::{DeckState, PendingRamp, RampParam};
pub use performative_parser::{EqBand, RampDuration};
use performative_parser::{Command, ParallelStep, Script, Statement, step_max_secs};

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
        for def in &["deck_player", "deck_eq", "master_mix"] {
            let path = synthdefs_dir.join(format!("{def}.scsyndef"));
            self.osc
                .send(msg::d_load(path.to_str().unwrap()))
                .await?;
        }
        // Give scsynth a moment to process the d_load messages.
        sleep(Duration::from_millis(150)).await;

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

        // Query buffer info to determine track duration. /b_info returns:
        //   [buf_num, num_frames, num_channels, sample_rate]
        // Types can vary (Int vs Long, Float vs Double) so extract with a helper.
        if let Ok(info) = self.osc.send_recv(msg::b_query(buf_num), "/b_info", 5_000).await {
            if info.args.len() >= 4 {
                let frames = osc_to_f32(&info.args[1]);
                let sr = osc_to_f32(&info.args[3]);
                if let (Some(f), Some(s)) = (frames, sr) {
                    if s > 0.0 {
                        self.state.lock().await.decks[deck_idx].track_duration = Some(f / s);
                    }
                }
            }
        }

        self.state.lock().await.status_msg =
            format!("Loaded: {track_name} → Deck {}", deck_idx + 1);
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
            Command::Quit => {
                self.state.lock().await.status_msg = "Type Esc or ctrl-c to quit.".into();
                Ok(())
            }
        }
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
