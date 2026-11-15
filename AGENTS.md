# MCPDoze - agent instructions

Rust workspace: an HTTP broker for MCP servers (shared single instance, lazy
spawn, idle TTL unload, HTTP passthrough). Plan: `docs/DEVELOPMENT_PLAN.md`.
Contribution rules: `CONTRIBUTING.md`.

## Rules
- This repository is PUBLIC. Never commit usernames, home paths, hostnames,
  emails, tokens, real MCP configs or logs from a developer machine. Use
  placeholders (`<home>`, `example`). Test fixtures use the mock servers in
  `crates/mcpdoze-testkit`.
- Commits: Conventional Commits, `git commit -s` (DCO). Do not push unless the
  owner asks.
- License GPL-3.0-or-later; SPDX header in every source file.
- Rust stable. `cargo fmt`, `cargo clippy -D warnings`, `cargo test` must pass.
- Before heavy builds or test runs check machine headroom; never run more than
  three heavy builds at once.
- Process-management code (Job Objects, process groups) is the only place
  `unsafe` is allowed; keep it in `proctree`, documented.
- Never log secrets or backend environment values.
