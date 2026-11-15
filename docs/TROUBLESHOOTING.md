# Troubleshooting

Start with `mcpdoze doctor`. It checks the config file, the token file (and its
permissions), whether the broker answers and accepts the token, whether the
port is free, whether each server's command is on `PATH`, and whether the
environment variables it references are set. Each line is `ok`, `warn` or
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
