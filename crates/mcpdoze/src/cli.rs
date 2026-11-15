// SPDX-License-Identifier: GPL-3.0-or-later

//! Command-line definitions. Each subcommand is one [`Command`] variant with
//! its own `Args` struct; client commands (status, add, remove, ...) are
//! added as siblings of [`Command::Serve`] and talk to the admin API
//! (`mcpdoze_core::admin::api`).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// MCPDoze: one shared, lazily started instance of each MCP server.
#[derive(Debug, Parser)]
#[command(name = "mcpdoze", version, about, propagate_version = true)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the broker in the foreground.
    Serve(ServeArgs),
}

/// `mcpdoze serve`.
#[derive(Debug, Args)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_serve() {
        let cli = Cli::try_parse_from([
            "mcpdoze",
            "serve",
            "--config",
            "example.toml",
            "--bind",
            "127.0.0.1:9000",
        ])
        .unwrap();
        let Command::Serve(a) = cli.command;
        assert_eq!(a.config, Some(PathBuf::from("example.toml")));
        assert_eq!(a.bind.as_deref(), Some("127.0.0.1:9000"));
        assert!(!a.no_watch);
        assert!(Cli::try_parse_from(["mcpdoze"]).is_err());
        assert!(Cli::try_parse_from(["mcpdoze", "serve", "--server", "x"]).is_err());
    }
}
