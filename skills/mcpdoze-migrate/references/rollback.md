# Rollback

Every `mcpdoze import` apply:

1. copies each file it will change to `<file>.mcpdoze-bak-<UTC timestamp>`;
2. writes a manifest, `<data dir>/import/<timestamp>.json`, listing the files,
   their backups, a SHA-256 of the original and of the new content, and the
   server names it registered.

`mcpdoze import --rollback <manifest>` restores the backups and removes the
registered servers, but only if each file still has the hash recorded after the
migration. If you edited a file since, it refuses and says which one.

Manual recovery if the binary is unavailable: copy each
`<file>.mcpdoze-bak-<timestamp>` over the file named in the manifest, then
remove the corresponding `[servers.<name>]` tables from the MCPDoze config and
run `mcpdoze reload` (or restart the broker).
