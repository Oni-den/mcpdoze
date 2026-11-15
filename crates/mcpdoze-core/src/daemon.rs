// SPDX-License-Identifier: GPL-3.0-or-later

//! The config-driven broker behind `mcpdoze serve`: load and validate the
//! config, obtain the bearer token, bind, register the configured servers,
//! hot-reload the config file on change, and stop every backend on shutdown.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::admin::Control;
use crate::auth::{self, Token};
use crate::backend::Tuning;
use crate::config::{self, BrokerConfig, Config, ConfigError};
use crate::http::{self, Security};
use crate::router::{Catalog, MemoryCatalog, Router};

/// Quiet period after a config-file event before reloading.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
/// Longest wait for open HTTP requests on shutdown before they are dropped.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// Token file name in the data directory when `token_file` is not set.
pub const DEFAULT_TOKEN_FILE: &str = "token";

/// How to run the broker.
#[derive(Clone)]
pub struct ServeOptions {
    /// Config file; `MCPDOZE_CONFIG` or the platform default when `None`.
    pub config_path: Option<PathBuf>,
    /// Overrides `broker.bind`.
    pub bind: Option<String>,
    /// Watch the config file and hot-reload it.
    pub watch: bool,
    /// Lifecycle timing.
    pub tuning: Tuning,
    /// Tool catalog (in-memory when `None`).
    pub catalog: Option<Arc<dyn Catalog>>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            bind: None,
            watch: true,
            tuning: Tuning::default(),
            catalog: None,
        }
    }
}

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeOptions")
            .field("config_path", &self.config_path)
            .field("bind", &self.bind)
            .field("watch", &self.watch)
            .finish_non_exhaustive()
    }
}

/// Why the broker could not start.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The config file could not be read or parsed.
    #[error("{path}: {source}")]
    Config {
        /// Config path.
        path: String,
        /// Cause.
        source: ConfigError,
    },
    /// The config is invalid (one problem per line).
    #[error("invalid config:\n{0}")]
    Invalid(String),
    /// No usable bearer token.
    #[error("bearer token: {0}")]
    Token(String),
    /// The bind address is invalid or cannot be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        /// Address.
        addr: String,
        /// Cause.
        source: io::Error,
    },
}

/// A running broker.
pub struct Daemon {
    addr: SocketAddr,
    token_file: Option<PathBuf>,
    control: Arc<Control>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
    watcher: Option<(notify::RecommendedWatcher, JoinHandle<()>)>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("addr", &self.addr)
            .field("token_file", &self.token_file)
            .finish_non_exhaustive()
    }
}

fn load_config(path: Option<&Path>) -> Result<Config, DaemonError> {
    let Some(path) = path else {
        tracing::warn!("no config path available; starting with no servers");
        return Ok(Config::default());
    };
    match Config::load(path) {
        Ok(c) => Ok(c),
        Err(ConfigError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "config file not found; starting with no servers");
            Ok(Config::default())
        }
        Err(source) => Err(DaemonError::Config {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The bearer token and the file it lives in (`None` for `token_env`).
/// A relative `token_file` is resolved against the platform data directory.
pub fn resolve_token(broker: &BrokerConfig) -> Result<(Token, Option<PathBuf>), DaemonError> {
    if let Some(var) = &broker.token_env {
        let value = std::env::var(var)
            .map_err(|_| DaemonError::Token(format!("environment variable {var} is not set")))?;
        let token = Token::from_string(value).ok_or_else(|| {
            DaemonError::Token(format!(
                "environment variable {var} is empty or contains whitespace"
            ))
        })?;
        return Ok((token, None));
    }
    let path = token_path(broker)?;
    let token = auth::load_or_create_token(&path)
        .map_err(|e| DaemonError::Token(format!("{}: {e}", path.display())))?;
    if let Some(w) = auth::warn_if_permissive(&path) {
        tracing::warn!("{w}");
    }
    Ok((token, Some(path)))
}

fn token_path(broker: &BrokerConfig) -> Result<PathBuf, DaemonError> {
    let rel = broker
        .token_file
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_FILE));
    if rel.is_absolute() {
        return Ok(rel);
    }
    let dir = config::default_data_dir()
        .ok_or_else(|| DaemonError::Token("cannot determine the data directory".into()))?;
    Ok(dir.join(rel))
}

fn parse_bind(bind: &str) -> Result<SocketAddr, DaemonError> {
    let bad = |m: &str| DaemonError::Bind {
        addr: bind.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, m.to_owned()),
    };
    if let Some(port) = bind.strip_prefix("localhost:") {
        let port: u16 = port.parse().map_err(|_| bad("invalid port"))?;
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    bind.parse().map_err(|_| bad("expected host:port"))
}

/// Watches the directory of `path` (editors replace files by rename) and
/// reloads after a quiet period. The event channel holds one pending
/// signal; bursts collapse into one reload.
fn watch_config(
    path: &Path,
    control: Arc<Control>,
) -> notify::Result<(notify::RecommendedWatcher, JoinHandle<()>)> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let _ = std::fs::create_dir_all(&dir);
    let file_name = path.file_name().map(|n| n.to_os_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        if ev
            .paths
            .iter()
            .any(|p| p.file_name().map(|n| n.to_os_string()) == file_name)
        {
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    let task = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            loop {
                tokio::time::sleep(RELOAD_DEBOUNCE).await;
                if rx.try_recv().is_err() {
                    break;
                }
            }
            match control.reload().await {
                Ok(r) => tracing::debug!(
                    added = r.added.len(),
                    removed = r.removed.len(),
                    changed = r.changed.len(),
                    "config reloaded"
                ),
                Err(e) => {
                    let details = match &e {
                        crate::admin::ControlError::Invalid(_, d) => d.join("; "),
                        _ => String::new(),
                    };
                    tracing::warn!(error = %e, %details, "config reload failed; keeping the running config");
                }
            }
        }
    });
    Ok((watcher, task))
}

impl Daemon {
    /// Loads the config, binds and starts serving.
    pub async fn start(opts: ServeOptions) -> Result<Daemon, DaemonError> {
        let path = opts
            .config_path
            .clone()
            .or_else(config::default_config_path);
        let mut cfg = load_config(path.as_deref())?;
        if let Some(b) = &opts.bind {
            cfg.broker.bind = b.clone();
        }
        cfg.validate().map_err(|errs| {
            DaemonError::Invalid(
                errs.iter()
                    .map(|e| format!("  {e}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })?;
        if let Some(p) = &path {
            if let Some(w) = auth::warn_if_permissive(p) {
                tracing::warn!("{w}");
            }
        }
        let (token, token_file) = resolve_token(&cfg.broker)?;
        match &token_file {
            Some(p) => tracing::info!(path = %p.display(), "bearer token file"),
            None => tracing::info!(
                var = cfg.broker.token_env.as_deref().unwrap_or_default(),
                "bearer token from environment variable"
            ),
        }
        let want = parse_bind(&cfg.broker.bind)?;
        let listener = http::bind(want).await.map_err(|source| DaemonError::Bind {
            addr: cfg.broker.bind.clone(),
            source,
        })?;
        let addr = listener.local_addr().map_err(|source| DaemonError::Bind {
            addr: cfg.broker.bind.clone(),
            source,
        })?;
        let catalog = opts
            .catalog
            .clone()
            .unwrap_or_else(|| Arc::new(MemoryCatalog::default()));
        let router = Router::with(catalog, opts.tuning);
        let security = Security::new(token, cfg.broker.allowed_hosts.clone());
        let control = Control::new(router, cfg, path.clone());
        let applied = control.apply_initial().await;
        tracing::info!(servers = applied.added.len(), "servers registered");

        let watcher = match (&path, opts.watch) {
            (Some(p), true) => match watch_config(p, control.clone()) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot watch the config file; hot reload disabled");
                    None
                }
            },
            _ => None,
        };
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(http::serve(listener, control.clone(), security, async {
            let _ = stop_rx.await;
        }));
        tracing::info!(%addr, "mcpdoze listening on /mcp");
        Ok(Daemon {
            addr,
            token_file,
            control,
            stop: Some(stop),
            task: Some(task),
            watcher,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The token file, unless the token comes from `token_env`.
    pub fn token_file(&self) -> Option<&Path> {
        self.token_file.as_deref()
    }

    /// The control plane (admin operations without HTTP).
    pub fn control(&self) -> &Arc<Control> {
        &self.control
    }

    /// Serves until `signal` resolves (or the server fails), then shuts
    /// down.
    pub async fn run_until(mut self, signal: impl Future<Output = ()>) -> io::Result<()> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        tokio::select! {
            _ = signal => {}
            res = &mut task => {
                self.stop_watcher();
                self.control.shutdown().await;
                return res.unwrap_or_else(|e| Err(io::Error::other(e)));
            }
        }
        self.task = Some(task);
        self.shutdown().await
    }

    fn stop_watcher(&mut self) {
        if let Some((watcher, task)) = self.watcher.take() {
            drop(watcher);
            task.abort();
        }
    }

    /// Stops accepting requests, waits up to [`SHUTDOWN_GRACE`] for open
    /// ones, and stops every backend (whole process trees).
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.stop_watcher();
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let mut res = Ok(());
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await {
                Ok(r) => res = r.unwrap_or_else(|e| Err(io::Error::other(e))),
                Err(_) => {
                    tracing::warn!("open requests did not finish; closing them");
                    task.abort();
                }
            }
        }
        // `serve` already stopped the backends unless it was aborted; this
        // is idempotent.
        self.control.shutdown().await;
        tracing::info!("mcpdoze stopped");
        res
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop_watcher();
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_parsing() {
        assert_eq!(
            parse_bind("localhost:7878").unwrap(),
            "127.0.0.1:7878".parse().unwrap()
        );
        assert_eq!(parse_bind("[::1]:0").unwrap(), "[::1]:0".parse().unwrap());
        assert!(parse_bind("localhost:x").is_err());
        assert!(parse_bind("nohost").is_err());
    }

    #[test]
    fn relative_token_file_goes_to_data_dir() {
        let b = BrokerConfig::default();
        if let Some(dir) = config::default_data_dir() {
            assert_eq!(token_path(&b).unwrap(), dir.join(DEFAULT_TOKEN_FILE));
        }
        let abs = std::env::temp_dir().join("mcpdoze-example-token");
        let b = BrokerConfig {
            token_file: Some(abs.clone()),
            ..BrokerConfig::default()
        };
        assert_eq!(token_path(&b).unwrap(), abs);
    }
}
