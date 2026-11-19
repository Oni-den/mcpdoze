# 0003: Deferred tool mode (technique 3c) is not built in v0.3

Status: deferred. Not implemented in v0.3. Revisit after real-use data exists (see Revisit below).

## Context

Technique 3c of the token-optimisation plan ([plans/M9-M10-plan.md](../plans/M9-M10-plan.md), section 3.3) would reduce the per-request schema overhead by listing only three meta-tools instead of every backend tool:

- `mcpdoze__search_tools`
- `mcpdoze__describe_tool`
- `mcpdoze__call_tool`

An agent would discover a tool with search, read its schema with describe, and run it through call_tool. It would be enabled per broker (`[broker] tool_mode = "search"`) or per server (`expose = "deferred"`).

The other optimisation techniques (allow/deny, description trimming, default arguments, result truncation, field filters, the session cache, TOON) change what a tool returns or how it is listed. This one changes the tool surface itself, and that has consequences the others do not.

Measurement is not yet available. The per-tool table (`mcpdoze stats --tools`, technique 0) exists, but it has not been run against real agent sessions, so the size of the list overhead in practice is unknown.

## Decision

Do not build deferred mode in v0.3. This record keeps the design open and the reasons on file; `docs/OPTIMIZE.md` marks the mode as not committed.

No code, configuration key or admin field for `tool_mode` or `expose` is added in this release, so those keys are unknown and rejected by the strict config parser (`deny_unknown_fields`).

## Revisit

Reopen this decision when both hold:

1. `mcpdoze stats --tools` from real sessions shows that the tool-list size (`list_bytes`, per server) is a significant share of the context an agent loads. Set a threshold before looking, for example a list over 10 % of the agent's tool budget across several sessions.
2. The agents the owner uses do not already provide native deferred or searchable tool loading. If they do, the mode gains nothing for them.

If it is built later, it needs its own design pass for the consequences below, and it must stay opt-in per server.

## Consequences and risks

- **Per-tool permissions.** Agent allowlists and approval prompts match tool names. With deferred mode the agent sees only `mcpdoze__call_tool`, so a rule such as "allow `issues__delete`" cannot match. Either every call is approved as one tool, or the allowlist is weakened to `call_tool`. This is the strongest reason to defer.
- **Output schemas lost.** The listed view of the three meta-tools cannot carry each backend tool's `outputSchema`. Agents that depend on `structuredContent` would need a describe step first.
- **No gain for agents with native tool search.** An agent that already loads tools lazily pays the extra round trip for nothing, and gets the permission problem above.
- **Extra round trips.** Each use needs search or describe before the call, which adds latency and tokens of its own. The savings are real only when the list is large and the agent rarely uses most tools.
- **Hidden tools.** A tool that is not listed does not appear in the agent's view of the server, so mistakes in search results are harder to diagnose. Errors from `call_tool` must name the underlying tool and say why it was refused.
- **Interaction with existing filters.** `tools_allow` and `tools_deny` (technique 3a) already reduce the list without this risk. Use them first.
