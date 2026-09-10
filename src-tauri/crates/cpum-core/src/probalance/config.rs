//! Configuration model and persistence: `probalance.json` (v1 envelope
//! format, atomic write).
//!
//! The GUI edits and saves; the service hot-reloads by checking mtime every
//! second - both processes share the same file, all reads and writes go
//! through this module, so the validation rules and version-rejection
//! logic only live in one place.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Configuration file schema version.
pub const PB_CONFIG_VERSION: u32 = 1;
/// Configuration file name (under `base_dir`).
pub const PB_CONFIG_FILE: &str = "probalance.json";

// =========================================================================
// Configuration model
// =========================================================================

/// ProBalance configuration (edited by the GUI, hot-loaded by the service,
/// persisted to `probalance.json`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProBalanceConfig {
    /// Master switch (off = engine idles; already-downgraded processes are
    /// restored immediately).
    #[serde(default)]
    pub enabled: bool,
    /// Foreground process CPU trigger threshold (single-core baseline %,
    /// 100 = one full core consumed).
    #[serde(default = "default_fg_threshold")]
    pub fg_cpu_threshold: f32,
    /// Background process CPU downgrade threshold (single-core baseline %).
    #[serde(default = "default_bg_threshold")]
    pub bg_cpu_threshold: f32,
    /// Seconds the contention must persist before downgrading (guards
    /// against transient spikes).
    #[serde(default = "default_sustain")]
    pub sustain_secs: u32,
    /// Seconds after contention clears before restoring (hysteresis, to
    /// avoid flapping).
    #[serde(default = "default_restore_after")]
    pub restore_after_secs: u32,
    /// Maximum downgrade duration per process (seconds); auto-restore on
    /// timeout (prevents forgotten downgrades).
    #[serde(default = "default_max_downgrade")]
    pub max_downgrade_secs: u64,
    /// User-supplied allowlist (process names; system-critical processes
    /// are already protected by the built-in list).
    #[serde(default)]
    pub whitelist: Vec<String>,
    /// Opt-in Game Mode. When the interactive desktop reports a fullscreen
    /// Direct3D presentation, the foreground process is boosted and the
    /// existing ProBalance policy suppresses eligible background work.
    #[serde(default)]
    pub game_mode_enabled: bool,
}

fn default_fg_threshold() -> f32 {
    100.0
}
fn default_bg_threshold() -> f32 {
    40.0
}
fn default_sustain() -> u32 {
    5
}
fn default_restore_after() -> u32 {
    15
}
fn default_max_downgrade() -> u64 {
    600
}

impl Default for ProBalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fg_cpu_threshold: default_fg_threshold(),
            bg_cpu_threshold: default_bg_threshold(),
            sustain_secs: default_sustain(),
            restore_after_secs: default_restore_after(),
            max_downgrade_secs: default_max_downgrade(),
            whitelist: vec![],
            game_mode_enabled: false,
        }
    }
}

impl ProBalanceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(10.0..=10000.0).contains(&self.fg_cpu_threshold) {
            return Err(format!("foreground trigger threshold must be between 10 and 10000: {}", self.fg_cpu_threshold));
        }
        if !(1.0..=10000.0).contains(&self.bg_cpu_threshold) {
            return Err(format!("background downgrade threshold must be between 1 and 10000: {}", self.bg_cpu_threshold));
        }
        if !(1..=600).contains(&self.sustain_secs) {
            return Err(format!("trigger sustain seconds must be between 1 and 600: {}", self.sustain_secs));
        }
        if !(1..=3600).contains(&self.restore_after_secs) {
            return Err(format!("restore-after seconds must be between 1 and 3600: {}", self.restore_after_secs));
        }
        if !(30..=86400).contains(&self.max_downgrade_secs) {
            return Err(format!("max downgrade seconds must be between 30 and 86400: {}", self.max_downgrade_secs));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct PbConfigFile {
    version: u32,
    config: ProBalanceConfig,
}

// =========================================================================
// Persistence
// =========================================================================

/// Load the configuration; a missing file returns the default (disabled).
pub fn load_config(base_dir: &Path) -> Result<ProBalanceConfig, String> {
    let path = base_dir.join(PB_CONFIG_FILE);
    if !path.exists() {
        return Ok(ProBalanceConfig::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("failed to read configuration: {e}"))?;
    let file: PbConfigFile = serde_json::from_str(&raw).map_err(|e| format!("failed to parse configuration: {e}"))?;
    if file.version > PB_CONFIG_VERSION {
        return Err(format!(
            "configuration file version {} is newer than the supported version {}, please upgrade the app",
            file.version, PB_CONFIG_VERSION
        ));
    }
    file.config.validate()?;
    Ok(file.config)
}

/// Save the configuration (validate + envelope format).
pub fn save_config(base_dir: &Path, config: &ProBalanceConfig) -> Result<(), String> {
    config.validate()?;
    if let Some(dir) = base_dir.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::create_dir_all(base_dir).map_err(|e| format!("failed to create directory: {e}"))?;
    let file = PbConfigFile { version: PB_CONFIG_VERSION, config: config.clone() };
    let json = serde_json::to_string_pretty(&file).map_err(|e| format!("failed to serialize configuration: {e}"))?;
    atomic_write(&base_dir.join(PB_CONFIG_FILE), &json)
}

/// Atomic file write: write to a same-named `.tmp` file first, then rename
/// to replace.
///
/// The service rewrites the status file every second and the GUI can read
/// it at any time. A direct `fs::write` lets readers hit half-written JSON
/// (the status panel flashes "service offline" and the config hot-reload
/// fails to parse until the next change). Rust's `fs::rename` on Windows
/// goes through `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which is an
/// atomic switch from the reader's perspective.
pub(super) fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).map_err(|e| format!("failed to write temp file: {e}"))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // Clean up leftovers; the next write recreates it.
            Err(format!("failed to atomically replace file: {e}"))
        }
    }
}
