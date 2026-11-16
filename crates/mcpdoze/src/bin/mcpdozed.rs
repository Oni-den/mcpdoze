// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdozed`: the broker without a console, for login autostart.
//!
//! Same broker as `mcpdoze serve` (same flags: `--config`, `--bind`,
//! `--no-watch`), but built for the Windows GUI subsystem, so no terminal
//! window opens. Standard streams are unusable there, so everything is
//! logged to a rotating file, `<log dir>/mcpdozed.log` (`broker.log_dir`,
//! else `<data dir>/logs`), panics included. Level: `MCPDOZE_LOG`.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use mcpdoze::logfile::{RotatingLog, DEFAULT_KEEP, DEFAULT_MAX_BYTES};
use mcpdoze::run::{self, ServeArgs};
use mcpdoze_core::config::Config;

/// MCPDoze broker as a background daemon (no console; logs to a file).
#[derive(Debug, Parser)]
#[command(name = "mcpdozed", version, about)]
struct DaemonCli {
    #[command(flatten)]
    serve: ServeArgs,
}

/// Writes `msg` to the default log location as a last resort (before the
/// configured log is open).
fn early_log(msg: &str) {
    if let Some(p) = run::daemon_log_path(None) {
        if let Ok(log) = RotatingLog::open(&p, DEFAULT_MAX_BYTES, DEFAULT_KEEP) {
            log.try_write(format!("mcpdozed: {msg}\n").as_bytes());
        }
    }
}

fn install_panic_hook(log: RotatingLog) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let line = format!("mcpdozed: PANIC {info}\nbacktrace:\n{bt}\n");
        if !log.try_write(line.as_bytes()) {
            // A log write holds the lock (the panic came from inside it):
            // fall back to a fresh handle on the same file.
            early_log(&line);
        }
        previous(info);
    }));
}

fn main() -> ExitCode {
    let cli = match DaemonCli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                let _ = e.print();
                return ExitCode::SUCCESS;
            }
            early_log(&e.to_string());
            let _ = e.print();
            return ExitCode::from(2);
        }
    };
    let cfg = cli.serve.config_path().and_then(|p| Config::load(&p).ok());
    let Some(path) = run::daemon_log_path(cfg.as_ref()) else {
        let _ = writeln!(
            std::io::stderr(),
            "mcpdozed: cannot determine the log directory"
        );
        return ExitCode::from(2);
    };
    let log = match RotatingLog::open(&path, DEFAULT_MAX_BYTES, DEFAULT_KEEP) {
        Ok(l) => l,
        Err(e) => {
            early_log(&format!("cannot open {}: {e}", path.display()));
            return ExitCode::from(2);
        }
    };
    install_panic_hook(log.clone());
    tracing_subscriber::fmt()
        .with_max_level(run::log_level())
        .with_ansi(false)
        .with_writer(log)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), log = %path.display(), "mcpdozed starting");
    let rt = match run::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run::serve(cli.serve.options())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{}", e.message);
            e.exit_code()
        }
    }
}
