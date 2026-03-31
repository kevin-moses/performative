// performative-analysis/src/lib.rs
//
// BPM, key, and beat detection for Performative tracks via a Python/Essentia
// subprocess.  The primary public surface is:
//
//   TrackAnalysis  — deserialized result from the analysis script
//   analyze()      — async function that checks a cache first, then shells out
//   analysis_cache_path() — returns the per-track cache file path

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;
use std::path::PathBuf;

// ── Public types ──────────────────────────────────────────────────────────────

/// Analysis result for a single track, produced by the Python/Essentia script.
///
/// All time values are in seconds.  The `key` string is formatted as
/// `"<note> <scale>"` (e.g. `"A minor"` or `"F# major"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackAnalysis {
    /// Detected BPM.
    pub bpm: f32,
    /// Key in the format "<note> <scale>" (e.g. "A minor").
    pub key: String,
    /// Beat positions in seconds from the start of the track.
    pub beats: Vec<f32>,
    /// Downbeat positions in seconds (every 4th beat starting from beat 0).
    pub downbeats: Vec<f32>,
    /// Total track duration in seconds.
    pub duration_secs: f32,
}

// ── Cache path ────────────────────────────────────────────────────────────────

/// Return the path where analysis results for `track_path` are cached.
///
/// Cache layout: `~/.performative/cache/<hash>/analysis.json`
/// The hash is a `DefaultHasher` hash of the canonical (absolute) path string.
/// If the path cannot be canonicalized the original string is hashed instead.
///
/// purpose: deterministically map a track path to a unique cache location.
/// @param track_path: the path string for the audio file (absolute or relative)
/// @return: PathBuf pointing to the analysis.json cache file
pub fn analysis_cache_path(track_path: &str) -> PathBuf {
    // Attempt to canonicalize so the hash is stable regardless of how the path
    // was specified.  Fall back to the raw string on canonicalize failure.
    let canonical = std::fs::canonicalize(track_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| track_path.to_string());

    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = hasher.finish();

    let home = home_dir();
    home.join(".performative")
        .join("cache")
        .join(hash.to_string())
        .join("analysis.json")
}

// ── Script discovery ──────────────────────────────────────────────────────────

/// Locate the `scripts/analyze.py` Python script.
///
/// Search order:
///   1. `scripts/analyze.py` relative to the running executable.
///   2. `scripts/analyze.py` relative to `CARGO_MANIFEST_DIR` (dev builds only).
///   3. `scripts/analyze.py` relative to the current working directory.
///
/// purpose: find the analysis script regardless of how the binary was invoked.
/// @return: Ok(PathBuf) if a readable file was found, Err otherwise
fn find_script() -> Result<PathBuf> {
    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();

        // 1. Relative to the running executable.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                v.push(dir.join("scripts").join("analyze.py"));
                // Also try one level up (common for target/debug/binary).
                if let Some(parent) = dir.parent() {
                    v.push(parent.join("scripts").join("analyze.py"));
                    if let Some(grandparent) = parent.parent() {
                        v.push(grandparent.join("scripts").join("analyze.py"));
                    }
                }
            }
        }

        // 2. CARGO_MANIFEST_DIR is only set at compile time; use env! here.
        //    We push the workspace root by navigating two levels up from the
        //    crate manifest directory (crates/performative-analysis → workspace root).
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        v.push(manifest_dir.join("..").join("..").join("scripts").join("analyze.py"));

        // 3. Current working directory.
        if let Ok(cwd) = std::env::current_dir() {
            v.push(cwd.join("scripts").join("analyze.py"));
        }

        v
    };

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not find scripts/analyze.py — tried relative to exe, CARGO_MANIFEST_DIR, and cwd"
    )
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Analyze a track, returning BPM, key, beat positions, and duration.
///
/// Checks the on-disk cache first (`~/.performative/cache/<hash>/analysis.json`).
/// On a cache miss, shells out to `uv run scripts/analyze.py <track_path>`,
/// parses the JSON output, writes it to the cache, and returns the result.
///
/// The deck is playable immediately — this function is intended to be called
/// inside a `tokio::spawn` so analysis runs in the background.
///
/// Returns a descriptive `Err` if:
///   - The file does not exist.
///   - uv is not available.
///   - Essentia cannot be installed by uv.
///   - The script produces invalid output.
///
/// purpose: async entry point for track analysis with transparent caching.
/// @param track_path: path to the audio file to analyze
/// @return: Ok(TrackAnalysis) on success, Err with context on failure
pub async fn analyze(track_path: &str) -> Result<TrackAnalysis> {
    // ── 1. Cache check ────────────────────────────────────────────────────────
    let cache_path = analysis_cache_path(track_path);

    if cache_path.exists() {
        let json = tokio::fs::read_to_string(&cache_path)
            .await
            .with_context(|| format!("failed to read cache file {}", cache_path.display()))?;
        let analysis: TrackAnalysis = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse cached analysis at {}", cache_path.display()))?;
        return Ok(analysis);
    }

    // ── 2. Verify the source file exists before shelling out ─────────────────
    if !std::path::Path::new(track_path).exists() {
        bail!("track file not found: {track_path}");
    }

    // ── 3. Locate the Python script ───────────────────────────────────────────
    let script_path = find_script()
        .context("analysis script not found — is the repository fully checked out?")?;

    // ── 4. Shell out to Python/Essentia ───────────────────────────────────────
    let output = tokio::process::Command::new("uv")
        .arg("run")
        .arg("--python")
        .arg(">=3.10,<3.14")
        .arg(&script_path)
        .arg(track_path)
        .output()
        .await
        .context("failed to launch uv — is uv installed? Install with: curl -LsSf https://astral.sh/uv/install.sh | sh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "analysis script exited with status {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let analysis: TrackAnalysis = serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "failed to parse analysis output as JSON: {}",
            stdout.trim()
        )
    })?;

    // ── 5. Write to cache ─────────────────────────────────────────────────────
    if let Some(cache_dir) = cache_path.parent() {
        tokio::fs::create_dir_all(cache_dir)
            .await
            .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;
    }

    let json_out = serde_json::to_string_pretty(&analysis)
        .context("failed to serialize analysis to JSON")?;

    tokio::fs::write(&cache_path, &json_out)
        .await
        .with_context(|| format!("failed to write cache file {}", cache_path.display()))?;

    Ok(analysis)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Return the user's home directory as a `PathBuf`.
///
/// purpose: resolve `~` without pulling in the `dirs` crate.
///          Uses `HOME` on Unix/macOS and `USERPROFILE` on Windows.
/// @return: PathBuf of the home directory, falling back to `/tmp` if unavailable
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TrackAnalysis serde roundtrip ─────────────────────────────────────────

    #[test]
    fn track_analysis_serde_roundtrip() {
        let original = TrackAnalysis {
            bpm: 128.5,
            key: "A minor".to_string(),
            beats: vec![0.5, 0.97, 1.44, 1.91],
            downbeats: vec![0.5, 2.38],
            duration_secs: 312.5,
        };

        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: TrackAnalysis =
            serde_json::from_str(&json).expect("deserialize failed");

        assert!((restored.bpm - original.bpm).abs() < 1e-4);
        assert_eq!(restored.key, original.key);
        assert_eq!(restored.beats.len(), original.beats.len());
        assert_eq!(restored.downbeats.len(), original.downbeats.len());
        assert!((restored.duration_secs - original.duration_secs).abs() < 1e-4);
    }

    #[test]
    fn track_analysis_serde_roundtrip_empty_beats() {
        let original = TrackAnalysis {
            bpm: 0.0,
            key: "C major".to_string(),
            beats: vec![],
            downbeats: vec![],
            duration_secs: 0.0,
        };

        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: TrackAnalysis =
            serde_json::from_str(&json).expect("deserialize failed");

        assert!(restored.beats.is_empty());
        assert!(restored.downbeats.is_empty());
    }

    #[test]
    fn track_analysis_deserialize_from_known_json() {
        let json = r#"{"bpm":128.0,"key":"F# major","beats":[0.5,0.97],"downbeats":[0.5],"duration_secs":240.0}"#;
        let analysis: TrackAnalysis =
            serde_json::from_str(json).expect("deserialize failed");

        assert!((analysis.bpm - 128.0).abs() < 1e-4);
        assert_eq!(analysis.key, "F# major");
        assert_eq!(analysis.beats.len(), 2);
        assert_eq!(analysis.downbeats.len(), 1);
        assert!((analysis.duration_secs - 240.0).abs() < 1e-4);
    }

    // ── analysis_cache_path ───────────────────────────────────────────────────

    #[test]
    fn analysis_cache_path_ends_with_analysis_json() {
        let path = analysis_cache_path("/some/track.mp3");
        assert_eq!(path.file_name().unwrap(), "analysis.json");
    }

    #[test]
    fn analysis_cache_path_contains_performative_cache() {
        let path = analysis_cache_path("/some/track.mp3");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(".performative") && path_str.contains("cache"),
            "expected path to contain .performative/cache, got: {path_str}"
        );
    }

    #[test]
    fn analysis_cache_path_different_tracks_have_different_hashes() {
        let path_a = analysis_cache_path("/tracks/song_a.mp3");
        let path_b = analysis_cache_path("/tracks/song_b.mp3");
        // The hash component is the parent directory name.
        let hash_a = path_a.parent().unwrap().file_name().unwrap();
        let hash_b = path_b.parent().unwrap().file_name().unwrap();
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn analysis_cache_path_same_track_is_deterministic() {
        let path1 = analysis_cache_path("/tracks/song.mp3");
        let path2 = analysis_cache_path("/tracks/song.mp3");
        assert_eq!(path1, path2);
    }

    // ── analyze() error path ──────────────────────────────────────────────────

    #[tokio::test]
    async fn analyze_returns_error_for_nonexistent_file() {
        let result = analyze("/nonexistent/path/that/does/not/exist/track.mp3").await;
        assert!(result.is_err(), "expected Err for nonexistent file, got Ok");
        let err_msg = result.unwrap_err().to_string();
        // The error message should mention the missing file, not panic.
        assert!(
            err_msg.contains("not found") || err_msg.contains("nonexistent"),
            "unexpected error message: {err_msg}"
        );
    }
}
