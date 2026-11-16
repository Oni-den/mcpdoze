// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze` binary. Command definitions live in [`cli`]; `serve` runs the
//! config-driven broker in the foreground (`mcpdoze::run`, shared with the
//! console-less `mcpdozed` daemon).
//!
//! Log level: `MCPDOZE_LOG` (`error`..`trace`, default `info`), written to
//! stderr. Logs never contain the bearer token or backend environment values.

mod autostart;
mod cli;
mod client;
mod commands;
mod hook;
mod import_cmd;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use mcpdoze::run;

use cli::{Cli, Command};

fn init_logging() {
    tracing_subscriber::fmt()
        .with_max_level(run::log_level())
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    let rt = match run::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mcpdoze: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        match cli.command {
            Command::Serve(args) => match run::serve(args.options()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("mcpdoze: {}", e.message);
                    e.exit_code()
                }
            },
            other => {
                let tty = std::io::stdout().is_terminal();
                match commands::run(other, &mut std::io::stdout(), tty).await {
                    Ok(code) => ExitCode::from(code),
                    Err(e) => {
                        eprintln!("mcpdoze: {e:#}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
    })
}
