# Security policy

## Reporting a vulnerability

Do not open a public issue for a security problem. Report it privately with
GitHub's private vulnerability reporting: open the repository's **Security**
tab and choose **Report a vulnerability**. Include the affected version or
commit, the platform, steps to reproduce, and the impact you expect.

We aim to acknowledge reports within a week and will keep you informed while
we work on a fix. Please allow a reasonable period for a fix before public
disclosure.

## Supported versions

MCPDoze is pre-release. Only the `main` branch receives security fixes until
the first tagged release; after that, the latest minor release will be
supported.

## Scope notes

- **Loopback by default.** The broker binds `127.0.0.1`. Binding any other
  address requires an explicit `--allow-remote` flag and a bearer token.
  Reports that a default installation is reachable from other hosts are in
  scope.
- **Host and Origin validation.** Requests with unexpected `Host` or `Origin`
  headers must be rejected. DNS-rebinding bypasses are in scope.
- **Bearer token.** The token is generated on first run into an owner-only
  file and compared in constant time. Any path that logs, echoes, or exposes
  the token outside that file is in scope.
- **Backend secrets.** Secrets are referenced by environment-variable name
  (`{ from_env = "NAME" }`). Literal secret-looking values are refused unless
  explicitly allowed. Leaking backend environment values is in scope.
- **Failure logs and export.** The failure log is off by default. Its records
  are built from types that cannot hold argument, result, environment or
  header values. `mcpdoze debug export` is designed to be safe to paste into a
  public issue, but review its output before posting it; a report that it
  leaks something is in scope. `mcpdoze logs` prints unscrubbed backend
  stderr and is **not** safe to paste. Raw frame capture is not available in
  this version.
- **Admin API.** Loopback-only regardless of the bind address.
- **Process trees.** Failures to terminate child processes, or orphaned
  processes after the broker exits, are in scope.
- **Out of scope:** the behaviour of third-party MCP servers that the broker
  launches, and vulnerabilities in agents that connect to the broker, unless
  MCPDoze amplifies them.

Do not include real tokens, configuration files, or logs from your own machine
in a report. Describe the problem with placeholders.
