use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Locations where scsynth might be found on macOS.
const SCSYNTH_CANDIDATES: &[&str] = &[
    "/Applications/SuperCollider.app/Contents/Resources/scsynth",
    "/usr/local/bin/scsynth",
    "/opt/homebrew/bin/scsynth",
];

pub struct Scsynth {
    process: Child,
    pub port: u16,
}

impl Scsynth {
    /// Locate the scsynth binary.
    pub fn find() -> Result<PathBuf> {
        for candidate in SCSYNTH_CANDIDATES {
            if Path::new(candidate).exists() {
                return Ok(PathBuf::from(candidate));
            }
        }
        // Fall back to PATH
        which_scsynth()
    }

    /// Boot scsynth on the given UDP port. Returns once the process is running
    /// (does NOT wait for audio-ready; call OscClient::wait_for_ready() after).
    /// Logs stdout+stderr to ~/.performative/scsynth.log for debugging.
    pub fn boot(port: u16) -> Result<Self> {
        let bin = Self::find().context(
            "scsynth not found. Install SuperCollider: brew install --cask supercollider",
        )?;

        let log_file = open_log_file()?;
        let log_copy = log_file.try_clone().context("failed to clone log file handle")?;

        // -u: UDP port  -b: max buffers  -z: block size  -m: real-time memory (KB)
        let process = Command::new(&bin)
            .args([
                "-u", &port.to_string(),
                "-b", "1024",
                "-z", "128",
                "-m", "65536",
            ])
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_copy))
            .spawn()
            .with_context(|| format!("failed to spawn scsynth at {}", bin.display()))?;

        Ok(Self { process, port })
    }

    pub fn log_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".performative").join("scsynth.log")
    }

    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Gracefully kill scsynth.
    pub fn quit(&mut self) {
        let _ = self.process.kill();
    }
}

impl Drop for Scsynth {
    fn drop(&mut self) {
        self.quit();
    }
}

fn open_log_file() -> Result<fs::File> {
    let path = Scsynth::log_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open scsynth log at {}", path.display()))
}

fn which_scsynth() -> Result<PathBuf> {
    let output = std::process::Command::new("which")
        .arg("scsynth")
        .output()
        .ok()
        .filter(|o| o.status.success());
    match output {
        Some(o) => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if path.is_empty() {
                anyhow::bail!("scsynth not found in PATH");
            }
            Ok(PathBuf::from(path))
        }
        None => anyhow::bail!("scsynth not found in PATH"),
    }
}
