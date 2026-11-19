// SPDX-License-Identifier: GPL-3.0-or-later
//! `[debug]` and `servers.*.debug`: the failure-diagnostics settings, and
//! the resolution of the effective mode per server (`docs/DEBUG.md` 4.5).
//!
//! Precedence, highest first: runtime (admin API, not persisted), CLI
//! (`serve --debug[=MODE] [--debug-server NAME]`), environment
//! (`MCPDOZE_DEBUG`), per-server (`servers.*.debug`), global (`[debug] mode`).
//! The default is `off`.
//!
//! Resolution is pure: [`resolve_debug`] turns a [`Config`] and the layers
//! into a [`DebugSettings`]. The admin control plane republishes it on every
//! apply, so a `[debug]` or `servers.*.debug` change never unloads a backend.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{de_duration, default_data_dir, Config, ValidationError};
use crate::diag::{DiagConfig, Level};

/// Environment variable with the env layer (`MCPDOZE_DEBUG`).
pub const ENV_VAR: &str = "MCPDOZE_DEBUG";

/// How much one server (or the whole broker) records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugMode {
    /// Nothing is recorded (default).
    #[default]
    Off,
    /// Failure records.
    Failures,
    /// Failures plus trace-only events (slow calls, noise, late responses).
    Trace,
}

impl DebugMode {
    /// `off`, `failures` or `trace`.
    pub fn as_str(self) -> &'static str {
        match self {
            DebugMode::Off => "off",
            DebugMode::Failures => "failures",
            DebugMode::Trace => "trace",
        }
    }

    /// The recorder level for this mode.
    pub fn level(self) -> Level {
        match self {
            DebugMode::Off => Level::Off,
            DebugMode::Failures => Level::Failures,
            DebugMode::Trace => Level::Trace,
        }
    }
}

impl FromStr for DebugMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(DebugMode::Off),
            "failures" => Ok(DebugMode::Failures),
            "trace" => Ok(DebugMode::Trace),
            _ => Err(format!(
                "unknown debug mode `{}` (use off, failures or trace)",
                s.trim()
            )),
        }
    }
}

/// `[debug]`: the global mode and the recorder settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugConfig {
    /// Mode for every server without its own `debug` key.
    #[serde(default)]
    pub mode: DebugMode,
    /// Debug directory; a relative path is resolved against the data
    /// directory. `<data dir>/debug` when left at the default.
    #[serde(default = "default_debug_dir")]
    pub dir: PathBuf,
    /// Size cap of one failure log file before it rotates.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// Rotated failure log files kept.
    #[serde(default = "default_keep")]
    pub keep: usize,
    /// Frames kept per backend instance for the failure record.
    #[serde(default = "default_frames")]
    pub frames: usize,
    /// Stderr lines attached to a record.
    #[serde(default = "default_stderr_lines")]
    pub stderr_lines: usize,
    /// Calls slower than this are recorded as `slow_call` (trace mode).
    #[serde(default = "default_slow_call", deserialize_with = "de_duration")]
    pub slow_call: Duration,
    /// Also record tool results with `isError`.
    #[serde(default)]
    pub tool_errors: bool,
    /// Raw frame capture (`docs/DEBUG.md` 4.4). Off by default.
    #[serde(default)]
    pub raw: bool,
    /// Raw capture switches itself off after this long.
    #[serde(default = "default_raw_for", deserialize_with = "de_duration")]
    pub raw_for: Duration,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            mode: DebugMode::Off,
            dir: default_debug_dir(),
            max_file_bytes: default_max_file_bytes(),
            keep: default_keep(),
            frames: default_frames(),
            stderr_lines: default_stderr_lines(),
            slow_call: default_slow_call(),
            tool_errors: false,
            raw: false,
            raw_for: default_raw_for(),
        }
    }
}

fn default_debug_dir() -> PathBuf {
    PathBuf::from("debug")
}
fn default_max_file_bytes() -> u64 {
    5 * 1024 * 1024
}
fn default_keep() -> usize {
    5
}
fn default_frames() -> usize {
    crate::diag::ring::DEFAULT_FRAMES
}
fn default_stderr_lines() -> usize {
    40
}
fn default_slow_call() -> Duration {
    Duration::from_secs(30)
}
fn default_raw_for() -> Duration {
    Duration::from_secs(30 * 60)
}

/// Largest accepted `[debug] frames`.
pub const MAX_FRAMES: usize = 4096;
/// Largest accepted `[debug] stderr_lines`.
pub const MAX_STDERR_LINES: usize = 1000;
/// Largest accepted `[debug] keep`.
pub const MAX_KEEP: usize = 100;

impl DebugConfig {
    /// The debug directory: absolute as written, a relative path under the
    /// platform data directory. `None` when no data directory exists.
    pub fn resolved_dir(&self) -> Option<PathBuf> {
        if self.dir.is_absolute() {
            Some(self.dir.clone())
        } else {
            default_data_dir().map(|d| d.join(&self.dir))
        }
    }

    pub(super) fn validate(&self, errs: &mut Vec<ValidationError>) {
        let field = |k: &str| format!("debug.{k}");
        if self.dir.as_os_str().is_empty() {
            errs.push(ValidationError::new(field("dir"), "must not be empty"));
        }
        if self.max_file_bytes < 4096 {
            errs.push(ValidationError::new(
                field("max_file_bytes"),
                "must be at least 4096",
            ));
        }
        if self.keep == 0 || self.keep > MAX_KEEP {
            errs.push(ValidationError::new(
                field("keep"),
                format!("must be between 1 and {MAX_KEEP}"),
            ));
        }
        if self.frames == 0 || self.frames > MAX_FRAMES {
            errs.push(ValidationError::new(
                field("frames"),
                format!("must be between 1 and {MAX_FRAMES}"),
            ));
        }
        if self.stderr_lines > MAX_STDERR_LINES {
            errs.push(ValidationError::new(
                field("stderr_lines"),
                format!("must be at most {MAX_STDERR_LINES}"),
            ));
        }
        if self.slow_call.is_zero() {
            errs.push(ValidationError::new(
                field("slow_call"),
                "must be greater than 0",
            ));
        }
        if self.raw {
            errs.push(ValidationError::new(
                field("raw"),
                "raw capture is not available in this version",
            ));
        }
        if self.raw_for.is_zero() {
            errs.push(ValidationError::new(
                field("raw_for"),
                "must be greater than 0",
            ));
        }
    }
}

/// One source of mode overrides: an optional level for every server and
/// explicit levels for named servers. A named server entry wins over `all`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugLayer {
    /// Mode for every server.
    pub all: Option<DebugMode>,
    /// Mode for specific servers.
    pub servers: BTreeMap<String, DebugMode>,
}

impl DebugLayer {
    /// The CLI layer of `serve --debug[=MODE] [--debug-server NAME]...`.
    /// With server names the mode (default `failures`) applies only to
    /// them; without, it applies to every server. A bare `--debug` means
    /// `failures`.
    pub fn from_cli(mode: Option<DebugMode>, servers: &[String]) -> DebugLayer {
        if servers.is_empty() {
            DebugLayer {
                all: mode,
                servers: BTreeMap::new(),
            }
        } else {
            let m = mode.unwrap_or(DebugMode::Failures);
            DebugLayer {
                all: None,
                servers: servers.iter().map(|s| (s.clone(), m)).collect(),
            }
        }
    }

    /// The environment layer from `MCPDOZE_DEBUG` (unset or empty: no
    /// overrides). Errors never echo the value.
    pub fn from_env() -> Result<DebugLayer, String> {
        match std::env::var(ENV_VAR) {
            Ok(v) => Self::from_env_value(&v),
            Err(std::env::VarError::NotPresent) => Ok(DebugLayer::default()),
            Err(_) => Err(format!("{ENV_VAR} is not valid unicode")),
        }
    }

    /// Parses an `MCPDOZE_DEBUG` value: a mode (applies to every server) or
    /// a comma list of server names (each at `failures`). Both may be mixed.
    pub fn from_env_value(value: &str) -> Result<DebugLayer, String> {
        let mut layer = DebugLayer::default();
        for raw in value.split(',') {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            if let Ok(mode) = tok.parse::<DebugMode>() {
                if let Some(prev) = layer.all {
                    if prev != mode {
                        return Err(format!(
                            "{ENV_VAR} names two different modes ({} and {})",
                            prev.as_str(),
                            mode.as_str()
                        ));
                    }
                }
                layer.all = Some(mode);
            } else if valid_env_name(tok) {
                layer.servers.insert(tok.to_owned(), DebugMode::Failures);
            } else {
                return Err(format!(
                    "{ENV_VAR} must be off, failures, trace or a comma list of server names"
                ));
            }
        }
        Ok(layer)
    }

    fn lookup(&self, server: &str) -> Option<DebugMode> {
        self.servers.get(server).copied().or(self.all)
    }
}

fn valid_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The runtime, CLI and environment layers, highest precedence first
/// (runtime, then CLI, then env).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugLayers {
    /// Set through the admin API (not persisted).
    pub runtime: DebugLayer,
    /// From `serve --debug`.
    pub cli: DebugLayer,
    /// From `MCPDOZE_DEBUG`.
    pub env: DebugLayer,
}

/// The effective debug settings of one configuration.
#[derive(Debug, Clone)]
pub struct DebugSettings {
    /// Global mode (`[debug] mode`).
    pub global: DebugMode,
    /// Recorder settings. `None` when the debug directory cannot be
    /// determined; every server then resolves to `off`.
    pub diag: Option<DiagConfig>,
    /// Raw frame capture requested by `[debug] raw`.
    pub raw: bool,
    /// Raw capture lifetime (`[debug] raw_for`).
    pub raw_for: Duration,
    per_server: BTreeMap<String, DebugMode>,
    layers: DebugLayers,
}

impl DebugSettings {
    /// The effective mode for `server`: runtime, CLI, env, per-server,
    /// then global. `off` when no debug directory is available.
    pub fn mode_for(&self, server: &str) -> DebugMode {
        if self.diag.is_none() {
            return DebugMode::Off;
        }
        let layered = [&self.layers.runtime, &self.layers.cli, &self.layers.env]
            .into_iter()
            .find_map(|l| l.lookup(server));
        layered
            .or_else(|| self.per_server.get(server).copied())
            .unwrap_or(self.global)
    }

    /// [`mode_for`](Self::mode_for) as a recorder level.
    pub fn level_for(&self, server: &str) -> Level {
        self.mode_for(server).level()
    }

    /// The layers this was resolved with.
    pub fn layers(&self) -> &DebugLayers {
        &self.layers
    }
}

/// Resolves the effective debug settings of `cfg` under `layers`.
pub fn resolve_debug(cfg: &Config, layers: &DebugLayers) -> DebugSettings {
    let d = &cfg.debug;
    let diag = d.resolved_dir().map(|dir| {
        let mut c = DiagConfig::new(dir);
        c.max_file_bytes = d.max_file_bytes;
        c.keep = d.keep;
        c.frames = d.frames;
        c.stderr_lines = d.stderr_lines;
        c.slow_call = d.slow_call;
        c.tool_errors = d.tool_errors;
        c
    });
    DebugSettings {
        global: d.mode,
        diag,
        raw: d.raw,
        raw_for: d.raw_for,
        per_server: cfg
            .servers
            .iter()
            .filter_map(|(n, s)| s.debug.map(|m| (n.clone(), m)))
            .collect(),
        layers: layers.clone(),
    }
}
