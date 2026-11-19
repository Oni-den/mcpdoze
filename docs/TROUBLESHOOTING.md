# Troubleshooting

Start with `mcpdoze doctor`. It checks the config file, the token file (and its
permissions), whether the broker answers and accepts the token, whether the
port is free, where the background daemon's log and the statistics log are
(and, on Windows, whether `mcpdozed.exe` is installed next to `mcpdoze.exe`),
whether each server's command is on `PATH`, and whether the environment
variables it references are set. Each line is `ok`, `warn` or
`FAIL`, and failures carry a `hint:`. It exits non-zero when a check failed.

```text
[ok  ] broker answers at http://127.0.0.1:7878
[FAIL] example: command `npx`: not found
       hint: install it or give the full path; remember the broker needs it on its own PATH
```

(Illustrative; the exact wording follows the checks above.)

## The broker is not reachable

Symptoms: `mcpdoze status` or the agent cannot connect.

- Is it running? Start it with `mcpdoze serve` (foreground) or
  `mcpdoze autostart install` (at login).
- Is the address right? The default is `127.0.0.1:7878`. Client commands find
  it from the config, `--url`, or `MCPDOZE_URL`. The agent entry must use the
  same address plus `/mcp`.
- Port in use: `doctor` reports `cannot bind ...`. Change `broker.bind` or stop
  the other program.
- Quick liveness check without a token: `GET /healthz` returns `ok`.
- Autostart not working: re-run `mcpdoze autostart install`, then check the
  task (Windows Task Scheduler), `systemctl --user status mcpdoze.service`
  (Linux) or the LaunchAgent (macOS). Variables the servers need must exist in
  the environment of that autostarted process, not only in your shell.

## Where is the log of the autostarted broker?

`mcpdoze autostart install` registers the console-less daemon `mcpdozed`
on Windows (no terminal window). It cannot write to a console, so it logs to
a file:

- `<data dir>/logs/mcpdozed.log`, or `mcpdozed.log` in `broker.log_dir` when
  set. `mcpdoze doctor` prints the exact path.
- The data directory is `%APPDATA%\mcpdoze\data` on Windows,
  `~/.local/share/mcpdoze` on Linux and `~/Library/Application Support/mcpdoze`
  on macOS.
- The file is rotated at 5 MB; `mcpdozed.log.1` .. `mcpdozed.log.5` keep the
  older parts. Panics are written there too, with a backtrace.
- More detail: set `MCPDOZE_LOG=debug` in the environment the task starts
  with, or run `mcpdoze serve` in a terminal to watch the log live (stop the
  task first: `schtasks /End /TN MCPDoze`).
- If the task still opens a console window, `mcpdozed.exe` was missing when
  autostart was installed: `install` then falls back to `mcpdoze serve` and
  says so. Put `mcpdozed.exe` (from the release archive, or
  `cargo install` builds it) next to `mcpdoze.exe` and run
  `mcpdoze autostart install` again; it replaces the existing task.

On Linux and macOS autostart keeps running `mcpdoze serve`: its output goes
to the journal (`journalctl --user -u mcpdoze.service`) or to
`~/Library/Logs/mcpdoze.log`.

## 401 or 403

- **401**: missing or wrong bearer token. The agent's header must be
  `Authorization: Bearer <token>`. `mcpdoze token` prints the token file path;
  load the file's contents into `MCPDOZE_TOKEN` in the shell that launches the
  agent, then restart the agent so it re-reads the variable. If you changed
  `broker.token_file` or `token_env`, restart the broker. A literal
  `${MCPDOZE_TOKEN}` that the agent did not expand also gives 401.
- **403**: a guard refused the request. Causes: a `Host` or `Origin` that is
  not `localhost`, `127.0.0.1`, `[::1]` or listed in `broker.allowed_hosts`
  (DNS-rebinding defence), or an admin API request from a non-loopback peer
  (admin is always loopback-only). Reaching the broker through another host
  name needs `allowed_hosts`.

## A backend is slow on first call

The first call to a server starts its process. Servers launched through `npx`
may download packages on first use, which can take longer than the default
`broker.start_timeout` of 60 s.

- Raise the limit: `start_timeout = "180s"` under `[broker]` (applies on
  reload).
- Install the package ahead of time so `npx` does not download at call time,
  or run it once by hand.
- Listing is never slow: `tools/list` is answered from the catalog and starts
  nothing. With `probe = true` (default) one throwaway start happens at
  registration, in the background, to fill the catalog.
- There is no `prewarm` setting. The first real call pays the start cost; a
  longer `idle_ttl` keeps the process alive between calls.
- `mcpdoze status` shows `starting`, `running` or `failed` with `last_error`;
  `mcpdoze logs <name>` shows the backend's last stderr lines.

## A server fails behind MCPDoze

When a server works on its own but fails through the broker, record why. The
failure log is off by default, so turn it on, reproduce, and read it.

1. **Enable.** `mcpdoze debug on` records classified failures for every
   server until the broker restarts. Narrow it with server names
   (`mcpdoze debug on example`), add slow and odd calls with `--trace`, and
   limit the time with `--for 30m`. (`--raw` is not available in this
   version.) `mcpdoze debug status` shows what is on.
2. **Reproduce.** Make the call that fails. Use the same agent and the same
   tool as before.
3. **Doctor.** `mcpdoze doctor` lists, per server with failures in the last
   24 hours, the count by kind, a likely cause (`hint:`) and the export
   command. With debug off but failures counted since the broker started it
   tells you how to enable it. `mcpdoze debug show` lists the records
   (`--server`, `--kind`, `--last N`).
4. **Export.** `mcpdoze debug export --server example` writes one markdown
   file (or use `--stdout`, `--out FILE`) with the launch shape (names only),
   the failure records, per-kind counts and latency, and a replay
   description. It reads the files directly, so it works with the broker
   down. Add `--since 7d` for a longer period, `--identify` to include the
   package argument of `npx`/`uvx` launchers, and `--strict` to drop every
   free-text field (messages, stderr tails, hints). Export scrubs a second
   time, re-keys the hashes, and checks its own output for home paths, user
   and host names, emails and token patterns; if anything is found it
   writes nothing and says `re-run with --strict`.
5. Turn it off again with `mcpdoze debug off`.

**`logs` versus `debug export`.** `mcpdoze logs <name>` shows the backend's
raw stderr, unscrubbed: it can hold paths, names and tokens, so read it
yourself and do not paste it. `mcpdoze debug export` is built to be pasted
into an issue (skim it first). Argument values are never recorded, only
their shape.

## Windows: `.cmd` shims

Tools such as `npx` are `.cmd` files on Windows. MCPDoze resolves commands with
`PATHEXT`, runs `.cmd`/`.bat` files through `cmd.exe`, and kills the whole
process tree on unload, so `command = "npx"` works. If `doctor` says a command
is not found, the broker's own `PATH` is the one that matters (an autostarted
broker may have a shorter `PATH` than your shell): put the full path in
`command`, or fix the environment the broker starts with.

## Other checks

- A server is not registered in `mcpdoze list`: the `error` field says why
  (`disabled`, or an environment variable that is not set for the broker).
- Tools do not appear in the agent: names are
  `mcp__mcpdoze__<prefix>__<tool>`; restart the agent or refresh its tool list.
  After a backend changes its tools run `mcpdoze refresh <name>`.
- A server needs per-session or per-directory instances: set `scope =
  "per-session"` or `"per-cwd"`; see [SCOPES.md](SCOPES.md). A call failing
  with `-32001` hit `max_instances`.
- Config rejected on reload: the error names the field (for example
  `servers.example.command`). Nothing was applied; fix it and run
  `mcpdoze reload`.
