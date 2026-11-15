// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze` binary.
//!
//! TEMPORARY CLI (M2/M3): just enough to exercise the data path end to end.
//! The real CLI and config-file driven `serve` replace this in M5.
//!
//! ```text
//! mcpdoze serve [--bind 127.0.0.1:7878] \
//!     --server <name>=<prefix>:<ttl>:<command> [args...] \
//!     [--server ...]
//! ```
//!
//! A server's arguments run until the next `--server` or `--bind`. `<ttl>` is
//! `500ms`, `30s`, `10m`, `1h` or plain seconds. Log level: `MCPDOZE_LOG`
//! (`error`..`trace`, default `info`), written to stderr.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::router::Router;

const USAGE: &str = "usage: mcpdoze serve [--bind 127.0.0.1:7878] \
--server <name>=<prefix>:<ttl>:<command> [args...] [--server ...]";

struct ServerArg {
    name: String,
    prefix: String,
    spec: BackendSpec,
}

fn parse_ttl(s: &str) -> Result<Duration, String> {
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num.parse().map_err(|_| format!("bad ttl {s:?}"))?;
    Ok(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return Err(format!("bad ttl unit in {s:?}")),
    })
}

fn parse_server(def: &str, args: Vec<String>) -> Result<ServerArg, String> {
    let (name, rest) = def
        .split_once('=')
        .ok_or_else(|| format!("--server {def:?}: expected <name>=<prefix>:<ttl>:<command>"))?;
    let mut parts = rest.splitn(3, ':');
    let (Some(prefix), Some(ttl), Some(command)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!(
            "--server {def:?}: expected <name>=<prefix>:<ttl>:<command>"
        ));
    };
    if name.is_empty() || command.is_empty() {
        return Err(format!("--server {def:?}: empty name or command"));
    }
    Ok(ServerArg {
        name: name.to_owned(),
        prefix: prefix.to_owned(),
        spec: BackendSpec::new(command, args).idle_ttl(parse_ttl(ttl)?),
    })
}

fn parse(args: &[String]) -> Result<(SocketAddr, Vec<ServerArg>), String> {
    let mut it = args.iter().peekable();
    match it.next().map(String::as_str) {
        Some("serve") => {}
        _ => return Err(USAGE.into()),
    }
    let mut bind: SocketAddr = "127.0.0.1:7878".parse().expect("static addr");
    let mut servers = Vec::new();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bind" => {
                let v = it.next().ok_or("--bind needs a value")?;
                bind = v.parse().map_err(|_| format!("bad --bind {v:?}"))?;
            }
            "--server" => {
                let def = it.next().ok_or("--server needs a value")?;
                let mut sargs = Vec::new();
                while let Some(n) = it.peek() {
                    if *n == "--server" || *n == "--bind" {
                        break;
                    }
                    sargs.push(it.next().expect("peeked").clone());
                }
                servers.push(parse_server(def, sargs)?);
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if !bind.ip().is_loopback() {
        return Err("only loopback binds are allowed until auth lands (M5)".into());
    }
    Ok((bind, servers))
}

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (bind, servers) = match parse(&args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let router = Router::new();
        for s in servers {
            if let Err(e) = router.register(&s.name, &s.prefix, s.spec) {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
        let listener = match mcpdoze_core::http::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bind {bind}: {e}");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(%bind, "mcpdoze listening on /mcp");
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        match mcpdoze_core::http::serve(listener, router, shutdown).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("serve: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_servers_and_args() {
        let (bind, servers) = parse(&v(&[
            "serve",
            "--server",
            "a=pa:30s:prog",
            "--tools",
            "x,y",
            "--bind",
            "127.0.0.1:9000",
            "--server",
            r"b=pb:5m:C:\tools\x.exe",
        ]))
        .unwrap();
        assert_eq!(bind.port(), 9000);
        assert_eq!(servers[0].spec.args, ["--tools", "x,y"]);
        assert_eq!(servers[0].spec.idle_ttl, Duration::from_secs(30));
        assert_eq!(servers[1].spec.command, r"C:\tools\x.exe");
        assert_eq!(servers[1].prefix, "pb");
    }

    #[test]
    fn rejects_remote_bind_and_bad_ttl() {
        assert!(parse(&v(&["serve", "--bind", "0.0.0.0:1"])).is_err());
        assert!(parse_ttl("5x").is_err());
        assert_eq!(parse_ttl("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_ttl("7").unwrap(), Duration::from_secs(7));
    }
}
