// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze` binary. Command definitions live in [`cli`]; `serve` runs the
//! config-driven broker from `mcpdoze_core::daemon`.
//!
//! Log level: `MCPDOZE_LOG` (`error`..`trace`, default `info`), written to
//! stderr. Logs never contain the bearer token or backend environment values.

mod cli;

use std::process::ExitCode;

use clap::Parser;
use mcpdoze_core::daemon::{Daemon, ServeOptions};

use cli::{Cli, Command, ServeArgs};

fn init_logging() {
    let level = std::env::var("MCPDOZE_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
}

/// Resolves on Ctrl-C, or SIGTERM (Unix) / console close (Windows).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
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

async fn serve(args: ServeArgs) -> ExitCode {
    let opts = ServeOptions {
        config_path: args.config,
        bind: args.bind,
        watch: !args.no_watch,
        ..ServeOptions::default()
    };
    let daemon = match Daemon::start(opts).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mcpdoze: {e}");
            return ExitCode::from(2);
        }
    };
    match daemon.run_until(shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcpdoze: serve: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mcpdoze: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        match cli.command {
            Command::Serve(args) => serve(args).await,
        }
    })
}
