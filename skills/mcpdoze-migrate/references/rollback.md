# Rollback

Every `mcpdoze import` apply:

1. copies each file it will change to `<file>.mcpdoze-bak-<UTC timestamp>`;
2. writes a manifest, `<data dir>/import/<timestamp>.json`, listing the files,
   their backups, a SHA-256 of the original and of the new content, the
   server names it registered and, for JSON agent configs, the migrated
   entries with their positions. Literal `env` / `headers` values and URLs
   with a query or credentials are stored as `<redacted: restored from
   backup>`; the manifest never holds a secret value.

`mcpdoze import --rollback <manifest>` (no agent argument) then:

- restores a file whole from its backup when it is unchanged since the
  migration;
- for a JSON agent config (Claude Code, Cursor, VS Code, OpenCode) that the
  agent has rewritten since, edits the current file: removes the `mcpdoze`
  entry the import added (or puts back the one that was there before),
  re-inserts the migrated entries at their old positions (redacted values are
  read from the verified backup), and keeps every other key and the key
  order. An entry you re-added yourself is left alone. It refuses, touching
  nothing, if the file is not valid JSON;
- refuses, touching nothing, when a Codex `config.toml` changed since the
  migration, or when a manifest from an older version (without saved
  entries) does not match;
- removes the registered servers from the MCPDoze config.

Plugin-provided servers (Claude Code plugins) are registered with the broker
but their plugin files are never in the manifest: an import does not change
them and a rollback does not restore them. If the user disabled plugin
servers in the agent after the import, they re-enable them there.

A manifest can be rolled back once; a second run is refused and changes
nothing.

Manual recovery if the binary is unavailable: copy each
`<file>.mcpdoze-bak-<timestamp>` over the file named in the manifest, then
remove the corresponding `[servers.<name>]` tables from the MCPDoze config and
run `mcpdoze reload` (or restart the broker).
