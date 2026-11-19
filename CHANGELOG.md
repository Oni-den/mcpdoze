# Changelog

## 0.3.0

### Failure diagnostics (M9)
- Opt-in failure recorder (`[debug]`, `serve --debug`, `MCPDOZE_DEBUG`, admin API) writing sanitised records to `failures.jsonl`; off by default, and `doctor`/`status` hint to enable it when failures were counted.
- `mcpdoze debug on|off|status|show|export`; scrubbed, self-checked export bundles and a testkit replay of them.
- Backend fixes: stdio call timeout, exit status surfaced, oversize frames fail pending calls, string ids accepted, wire noise counted.
- Leak-review fixes in the scrubber, export self-check and stderr capture.
- Raw frame capture is not available: `[debug] raw = true` is rejected.

### Token optimisation (M10)
- Per-tool size and duplicate-call statistics (`stats --tools`).
- Per-server opt-in `optimize` and `tools` tables: tool allow/deny, description trimming, `default_args`, JSON minify, result truncation with `mcpdoze__result_get`, field filters, session result cache (read-only tools only), lossless TOON encoding, and the `mcpdoze__batch` meta-tool (`off`, `read-only`, `all`).
- Servers without an `optimize` table behave as before. Deferred tool mode is not built (ADR 0003).
