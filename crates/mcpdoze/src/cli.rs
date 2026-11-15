// SPDX-License-Identifier: GPL-3.0-or-later

//! Command-line definitions. Each subcommand is one [`Command`] variant with
//! its own `Args` struct; client commands (status, add, remove, ...) talk to
//! the admin API (`mcpdoze_core::admin::api`) through [`crate::client`].

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Show the runtime state of every server.
    Status(StatusArgs),
    /// List configured servers.
    List(ListArgs),
    /// Register a server (stdio command or remote HTTP endpoint).
    Add(AddArgs),
    /// Remove a server from the config and unload it.
    Remove(NameArgs),
    /// Unload a server's backend; it starts again on the next call.
    Stop(StopArgs),
    /// Re-read the config file and apply the difference.
    Reload(ConnArgs),
    /// Refresh the cached tool catalog of one server, or of all.
    Refresh(RefreshArgs),
    /// Show the last stderr lines of a backend.
    Logs(LogsArgs),
    /// Print the path of the bearer token file.
    Token(TokenArgs),
    /// Check the installation and explain how to fix problems.
    Doctor(ConnArgs),
    /// Start the broker automatically at login.
    Autostart {
        /// Install or remove.
        #[command(subcommand)]
        action: AutostartCmd,
    },
    /// Move MCP server entries from another agent's config into MCPDoze.
    Import(ImportCmdArgs),
    /// Optional Claude Code hook that redirects native MCP installs to `mcpdoze add`.
    Hook {
        /// What to do.
        #[command(subcommand)]
        action: HookAction,
    },
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

/// How a client command finds the broker.
#[derive(Debug, Clone, Default, Args)]
pub struct ConnArgs {
    /// Config file used to find the broker address and token
    /// [default: $MCPDOZE_CONFIG, else <config dir>/mcpdoze/config.toml].
    #[arg(long, value_name = "PATH", env = "MCPDOZE_CONFIG")]
    pub config: Option<PathBuf>,
    /// Broker base URL [default: from the config, else http://127.0.0.1:7878].
    #[arg(long, alias = "broker-url", value_name = "URL", env = "MCPDOZE_URL")]
    pub url: Option<String>,
    /// Token file [default: from the config, else <data dir>/token].
    #[arg(long, value_name = "PATH", env = "MCPDOZE_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
}

/// `mcpdoze status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
    /// Print the raw JSON response.
    #[arg(long)]
    pub json: bool,
}

/// `mcpdoze list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
    /// Print the raw JSON response.
    #[arg(long)]
    pub json: bool,
}

/// A command that takes one server name.
#[derive(Debug, Args)]
pub struct NameArgs {
    /// Server name.
    pub name: String,
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
}

/// `mcpdoze stop`.
#[derive(Debug, Args)]
pub struct StopArgs {
    /// Server name.
    pub name: String,
    /// Kill the process tree at once instead of draining calls in flight.
    #[arg(long)]
    pub force: bool,
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
}

/// `mcpdoze refresh`.
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Server name (all servers when omitted).
    pub name: Option<String>,
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
}

/// `mcpdoze logs`.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Server name.
    pub name: String,
    /// Number of lines (at most 500).
    #[arg(short = 'n', long, default_value_t = 100, value_name = "LINES")]
    pub lines: usize,
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
}

/// `mcpdoze token`.
#[derive(Debug, Args)]
pub struct TokenArgs {
    /// Connection options.
    #[command(flatten)]
    pub conn: ConnArgs,
    /// Print the token itself (only when stdout is a terminal).
    #[arg(long)]
    pub show: bool,
}

/// Transport of a server being added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportArg {
    /// Local process speaking MCP over stdin/stdout.
    Stdio,
    /// Remote Streamable HTTP endpoint.
    Http,
}

/// Instance scope of a server being added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    /// One instance for every agent.
    Shared,
    /// One instance per working directory.
    #[value(name = "per-cwd")]
    PerCwd,
    /// One instance per agent session.
    #[value(name = "per-session")]
    PerSession,
}

/// `mcpdoze add <name> [options] -- <command> [args...]`.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// Server name (`[A-Za-z0-9_.-]`, at most 64 characters).
    pub name: String,
    /// Transport.
    #[arg(long, value_enum, default_value = "stdio")]
    pub transport: TransportArg,
    /// Tool prefix [default: the sanitised name].
    #[arg(long)]
    pub prefix: Option<String>,
    /// Instance scope.
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    /// Idle time before the backend is unloaded (e.g. 5m, 90s).
    #[arg(long, value_name = "DURATION")]
    pub ttl: Option<String>,
    /// Backend environment variable taken from the broker's environment:
    /// NAME=ENVVAR (repeatable). The value is never stored.
    #[arg(long, value_name = "NAME=ENVVAR")]
    pub env: Vec<String>,
    /// Backend environment variable with a literal value: NAME=VALUE
    /// (repeatable; needs --allow-literal-env).
    #[arg(long, value_name = "NAME=VALUE", requires = "allow_literal_env")]
    pub env_literal: Vec<String>,
    /// Permit literal values in the config instead of references.
    #[arg(long)]
    pub allow_literal_env: bool,
    /// Working directory of the backend.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Remote endpoint URL (http transport).
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
    /// Header sent to the remote: NAME=ENVVAR (repeatable).
    #[arg(long, value_name = "NAME=ENVVAR")]
    pub header: Vec<String>,
    /// Config file used to find the broker address and token.
    #[arg(long, value_name = "PATH", env = "MCPDOZE_CONFIG")]
    pub config: Option<PathBuf>,
    /// Broker base URL.
    #[arg(long, value_name = "URL", env = "MCPDOZE_URL")]
    pub broker_url: Option<String>,
    /// Token file.
    #[arg(long, value_name = "PATH", env = "MCPDOZE_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
    /// Command and arguments (after `--`).
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

impl AddArgs {
    /// The broker connection options.
    pub fn conn(&self) -> ConnArgs {
        ConnArgs {
            config: self.config.clone(),
            url: self.broker_url.clone(),
            token_file: self.token_file.clone(),
        }
    }
}

/// `mcpdoze autostart <install|remove>`.
#[derive(Debug, Subcommand)]
pub enum AutostartCmd {
    /// Register the broker to start at login.
    Install {
        /// Config file passed to `serve`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Listen address passed to `serve`.
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
    },
    /// Remove the autostart registration.
    Remove,
}

/// `mcpdoze import`.
#[derive(Debug, Args)]
pub struct ImportCmdArgs {
    /// Agent config to import from (agent name or file path).
    #[arg(required_unless_present = "rollback", value_name = "FROM")]
    pub from: Option<String>,
    /// Show what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Undo a previous import, given the manifest path it printed
    /// (`mcpdoze import --rollback <manifest>`; no agent argument needed).
    #[arg(long, value_name = "MANIFEST")]
    pub rollback: Option<PathBuf>,
    /// MCPDoze config file to add the servers to.
    #[arg(long, value_name = "PATH", env = "MCPDOZE_CONFIG")]
    pub config: Option<PathBuf>,
    /// Broker MCP endpoint the agent will be pointed at.
    #[arg(long, value_name = "URL", default_value = "http://127.0.0.1:7878/mcp")]
    pub broker_url: String,
    /// Instance scope for imported servers.
    #[arg(long)]
    pub scope: Option<String>,
    /// Print the plan or result as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `mcpdoze hook ...`.
#[derive(Debug, Subcommand)]
pub enum HookAction {
    /// Run as a Claude Code PreToolUse hook (reads the event on stdin).
    ClaudeCode,
    /// Add the hook to a Claude Code settings file.
    Install(HookInstallArgs),
    /// Remove the hook from a Claude Code settings file.
    Remove(HookInstallArgs),
}

/// Agents the hook supports.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookAgent {
    /// Claude Code.
    ClaudeCode,
}

/// Where the settings file lives.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookScope {
    /// `<home>/.claude/settings.json`.
    User,
    /// `./.claude/settings.json`.
    Project,
}

/// Flags of `hook install` and `hook remove`.
#[derive(Debug, Args)]
pub struct HookInstallArgs {
    /// Agent to configure.
    #[arg(long, value_enum, default_value = "claude-code")]
    pub agent: HookAgent,
    /// Settings scope.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: HookScope,
    /// Settings file to edit (overrides --scope).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,
    /// Show what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        let mut v = vec!["mcpdoze"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v).unwrap().command
    }

    #[test]
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_serve() {
        let Command::Serve(a) = parse(&[
            "serve",
            "--config",
            "example.toml",
            "--bind",
            "127.0.0.1:9000",
        ]) else {
            panic!()
        };
        assert_eq!(a.config, Some(PathBuf::from("example.toml")));
        assert_eq!(a.bind.as_deref(), Some("127.0.0.1:9000"));
        assert!(!a.no_watch);
        assert!(Cli::try_parse_from(["mcpdoze"]).is_err());
        assert!(Cli::try_parse_from(["mcpdoze", "serve", "--server", "x"]).is_err());
    }

    #[test]
    fn parses_add_with_command() {
        let Command::Add(a) = parse(&[
            "add",
            "example",
            "--prefix",
            "ex",
            "--scope",
            "per-cwd",
            "--ttl",
            "5m",
            "--env",
            "API_KEY=EXAMPLE_API_KEY",
            "--cwd",
            "work",
            "--",
            "npx",
            "-y",
            "some-mcp-server",
        ]) else {
            panic!()
        };
        assert_eq!(a.name, "example");
        assert_eq!(a.transport, TransportArg::Stdio);
        assert_eq!(a.scope, Some(ScopeArg::PerCwd));
        assert_eq!(a.env, vec!["API_KEY=EXAMPLE_API_KEY"]);
        assert_eq!(a.command, vec!["npx", "-y", "some-mcp-server"]);
    }

    #[test]
    fn parses_add_http_and_literal_gate() {
        let Command::Add(a) = parse(&[
            "add",
            "remote",
            "--transport",
            "http",
            "--url",
            "https://example.com/mcp",
            "--header",
            "Authorization=REMOTE_AUTH",
        ]) else {
            panic!()
        };
        assert_eq!(a.transport, TransportArg::Http);
        assert_eq!(a.url.as_deref(), Some("https://example.com/mcp"));
        assert!(a.command.is_empty());
        assert!(Cli::try_parse_from(["mcpdoze", "add", "x", "--env-literal", "A=b"]).is_err());
        assert!(Cli::try_parse_from([
            "mcpdoze",
            "add",
            "x",
            "--env-literal",
            "A=b",
            "--allow-literal-env",
            "--",
            "cmd"
        ])
        .is_ok());
    }

    #[test]
    fn parses_client_commands() {
        let Command::Stop(a) = parse(&["stop", "example", "--force", "--url", "http://h:1"]) else {
            panic!()
        };
        assert!(a.force);
        assert_eq!(a.conn.url.as_deref(), Some("http://h:1"));
        let Command::Logs(a) = parse(&["logs", "example", "-n", "20"]) else {
            panic!()
        };
        assert_eq!(a.lines, 20);
        let Command::Refresh(a) = parse(&["refresh"]) else {
            panic!()
        };
        assert!(a.name.is_none());
        let Command::Token(a) = parse(&["token", "--show", "--token-file", "t"]) else {
            panic!()
        };
        assert!(a.show);
        assert!(matches!(parse(&["status", "--json"]), Command::Status(s) if s.json));
        assert!(matches!(parse(&["doctor"]), Command::Doctor(_)));
        assert!(matches!(parse(&["reload"]), Command::Reload(_)));
    }

    #[test]
    fn parses_import_and_autostart() {
        let Command::Import(a) = parse(&["import", "agent-a", "--dry-run", "--scope", "shared"])
        else {
            panic!()
        };
        assert_eq!(a.from.as_deref(), Some("agent-a"));
        assert!(a.dry_run);
        assert!(Cli::try_parse_from(["mcpdoze", "import"]).is_err());
        assert!(Cli::try_parse_from(["mcpdoze", "import", "--rollback", "b.bak"]).is_ok());
        let Command::Import(r) = parse(&["import", "--rollback", "m.json"]) else {
            panic!()
        };
        assert!(r.from.is_none());
        assert_eq!(r.rollback.as_deref(), Some(std::path::Path::new("m.json")));
        assert!(matches!(
            parse(&["autostart", "install", "--bind", "127.0.0.1:1"]),
            Command::Autostart {
                action: AutostartCmd::Install { .. }
            }
        ));
        assert!(matches!(
            parse(&["autostart", "remove"]),
            Command::Autostart {
                action: AutostartCmd::Remove
            }
        ));
    }

    #[test]
    fn version_flag_exists() {
        let e = Cli::try_parse_from(["mcpdoze", "--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
