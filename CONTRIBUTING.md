# Contributing to MCPDoze

Thanks for helping. MCPDoze is licensed under **GPL-3.0-or-later**. By
contributing you agree your work is released under that license.

## Ground rules

- Be respectful and constructive. Assume good faith.
- Open an issue before starting a large change so the design can be agreed.
- Security problems: do **not** open a public issue. Use GitHub's private
  vulnerability reporting for this repository.
- **Never commit personal or machine-specific data**: usernames, home
  directory paths, hostnames, tokens, API keys, email addresses, real
  configuration files. Use placeholders such as `<home>` or `example`. CI runs a
  check that fails on such strings; run it locally before pushing.

## Developer Certificate of Origin (DCO)

Every commit must be signed off, certifying you have the right to submit it
under the project license (<https://developercertificate.org>):

```text
git commit -s -m "feat: short summary"
```

This appends `Signed-off-by: Your Name <you@example.com>`. Use an email you are
comfortable making public (a GitHub `noreply` address is fine). Unsigned commits
fail CI.

## Development setup

1. Install the stable Rust toolchain (`rustup`).
2. Fork and clone the repository, then create a branch from `main`:
   `git switch -c feat/my-change`.
3. Build and test:

   ```text
   cargo build --workspace
   cargo test --workspace
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   ```

Supported platforms are Windows (x64 and ARM64), Linux and macOS. Changes that
touch process management must be tested on Windows and on a Unix system; say in
the pull request what you ran.

## Code guidelines

- Rust, stable toolchain, `#![forbid(unsafe_code)]` except in the small,
  documented modules that call OS APIs (Job Objects, process groups).
- No panics on input from agents or backends; return JSON-RPC errors.
- Never log secrets, tokens, or environment values.
- Add tests with every behaviour change. Lifecycle and routing code needs tests
  for the race conditions it handles (call arriving during unload, crash during
  call, client disconnect during call).
- Keep dependencies minimal; new ones need a reason in the pull request and must
  pass `cargo deny`.
- Public items get doc comments. Each source file starts with an SPDX header:
  `// SPDX-License-Identifier: GPL-3.0-or-later`.

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org):
  `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`.
- Keep pull requests focused; one logical change each.
- Describe what changed and why, how you tested it, and any platform caveats.
- A maintainer review is required before merge. Resolve review threads rather
  than force-pushing over them; squash happens at merge.

## Adding support for another agent

Agent adapters for the migration skill live under `skills/` and `crates/`.
Each adapter must: detect configuration locations, support `--dry-run`, back up
every file it changes, and be reversible through the recorded manifest.

## Questions

Open a GitHub Discussion or issue. Thank you for contributing.
