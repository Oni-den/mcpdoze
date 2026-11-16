// SPDX-License-Identifier: GPL-3.0-or-later

//! Running the broker: shared by `mcpdoze serve` (foreground, logs to
//! stderr) and `mcpdozed` (background, no console, logs to a rotating file).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use mcpdoze_core::config::{self, Config};
use mcpdoze_core::daemon::{Daemon, ServeOptions};
use tracing_subscriber::filter::LevelFilter;

/// Log directory name in the data directory when `broker.log_dir` is unset.
pub const DEFAULT_LOG_DIR: &str = "logs";
/// File name of the background daemon's log.
pub const DAEMON_LOG_FILE: &str = "mcpdozed.log";

/// Options of `mcpdoze serve` and `mcpdozed`.
#[derive(Debug, Clone, Default, Args)]
pub struct ServeArgs {
    /// Config file [default: $MCPDOZE_CONFIG, else <config dir>/mcpdoze/config.toml].
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Listen address, overriding `broker.bind` (e.g. 127.0.0.1:7878).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Do not watch the config file for changes.
    #[arg(long)]
    pub no_watch: bool,
}

impl ServeArgs {
    /// The daemon options these flags select.
    pub fn options(&self) -> ServeOptions {
        ServeOptions {
            config_path: self.config.clone(),
            bind: self.bind.clone(),
            watch: !self.no_watch,
            ..ServeOptions::default()
        }
    }

    /// The config file these flags select (explicit, else the default).
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config.clone().or_else(config::default_config_path)
    }
}

/// Why the broker stopped with an error: exit code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeError {
    /// Process exit code.
    pub code: u8,
    /// One-line (or multi-line, for config errors) description.
    pub message: String,
}

impl ServeError {
    /// The process exit code.
    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

/// Log level from `MCPDOZE_LOG` (`error`..`trace`, default `info`).
pub fn log_level() -> LevelFilter {
    std::env::var("MCPDOZE_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(LevelFilter::INFO)
}

/// The Tokio runtime both binaries use.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}

/// The directory of broker log files: `broker.log_dir` (a relative path
/// is resolved against the data directory), else `<data dir>/logs`.
pub fn log_dir(cfg: Option<&Config>) -> Option<PathBuf> {
    match cfg.and_then(|c| c.broker.log_dir.clone()) {
        Some(p) if p.is_absolute() => Some(p),
        Some(p) => config::default_data_dir().map(|d| d.join(p)),
        None => config::default_data_dir().map(|d| d.join(DEFAULT_LOG_DIR)),
    }
}

/// Where `mcpdozed` writes its log.
pub fn daemon_log_path(cfg: Option<&Config>) -> Option<PathBuf> {
    log_dir(cfg).map(|d| d.join(DAEMON_LOG_FILE))
}

/// Resolves on Ctrl-C, or SIGTERM (Unix) / console close (Windows). Without
/// a console (Windows `mcpdozed`) the handlers may be unavailable; the
/// process is then ended by Task Scheduler, and the job objects take the
/// backends down with it.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(windows)]
    let term = async {
        match tokio::signal::windows::ctrl_close() {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutdown requested");
}

/// Starts the broker and serves until a shutdown signal.
pub async fn serve(opts: ServeOptions) -> Result<(), ServeError> {
    let daemon = Daemon::start(opts).await.map_err(|e| ServeError {
        code: 2,
        message: e.to_string(),
    })?;
    daemon
        .run_until(shutdown_signal())
        .await
        .map_err(|e| ServeError {
            code: 1,
            message: format!("serve: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_log_follows_log_dir() {
        let abs = std::env::temp_dir().join("mcpdoze-example-logs");
        let cfg = Config {
            broker: config::BrokerConfig {
                log_dir: Some(abs.clone()),
                ..config::BrokerConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(daemon_log_path(Some(&cfg)), Some(abs.join(DAEMON_LOG_FILE)));
        if let Some(d) = config::default_data_dir() {
            assert_eq!(
                daemon_log_path(None),
                Some(d.join("logs").join("mcpdozed.log"))
            );
        }
    }

    #[test]
    fn serve_args_map_to_options() {
        let a = ServeArgs {
            config: Some("example.toml".into()),
            bind: Some("127.0.0.1:1".into()),
            no_watch: true,
        };
        let o = a.options();
        assert_eq!(o.config_path, Some(PathBuf::from("example.toml")));
        assert_eq!(o.bind.as_deref(), Some("127.0.0.1:1"));
        assert!(!o.watch);
    }
}
