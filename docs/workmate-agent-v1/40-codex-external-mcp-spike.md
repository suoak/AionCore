# Spike: blocking external (machine-ambient) MCP for Codex sessions

## Status

```text
TYPE      = SPIKE (conclusions only, NO behavior change)
SCOPE     = M7 / E1 (B-09 follow-up for the codex backend)
CODE REF  = crates/aionui-session/src/backend/codex_conn.rs — thread_start_params (~L569),
            codex_app_server_args (~L62), mcpServer/startupStatus/updated (~L3134)
VERIFIED  = code reading only; every "needs live probe" item below is UNVERIFIED
```

## Problem

B-09 makes a *managed* MCP surface (Agent Center assistant, or an explicit MCP
allowlist — empty included, `extra.mcp_strict`) strict for Claude: the session is
launched with `--mcp-config <allowlist> --strict-mcp-config`, and an empty
allowlist becomes `{"mcpServers":{}}`, so no ambient `~/.claude` server can reach it.

Codex has no equivalent flag. Today WorkMate injects the allowlist through the
`thread/start` / `thread/resume` / `thread/fork` `config.mcp_servers` overlay
(`thread_start_params`). That overlay is **additive**: codex layers it on top of
`$CODEX_HOME/config.toml` (and any project `.codex/config.toml`), so every server the
user configured on the machine is still started and its tools are still offered to
the model. An empty allowlist sends no `config` key at all, which means "ambient
only". For a managed Codex session this means the allowlist is a floor, not a
ceiling.

## Options

| # | Option | How | Blocks ambient? | Main risks |
|---|--------|-----|-----------------|------------|
| A | Per-server `enabled = false` overlay | Enumerate configured servers, then send `config.mcp_servers.<name>.enabled = false` for every name outside the allowlist (thread overlay, or process-level `-c mcp_servers.<name>.enabled=false` in `codex_app_server_args`). | Yes, for the names we enumerate. | Needs the ambient name list: either parse `$CODEX_HOME/config.toml` + project config ourselves (TOML layering/profile drift), or ask codex (`mcpServerStatus/list`) first. `enabled` must be honored by the pinned codex version (needs live probe). A server added to config after the session started is not covered until respawn. |
| B | Isolated `CODEX_HOME` per managed session | Spawn the app-server with `CODEX_HOME` pointing at a WorkMate-owned dir holding a generated `config.toml` (allowlist only) and the user's auth. | Yes, completely (user `config.toml` never read). Project `.codex/config.toml` still needs checking. | Auth: `auth.json` must be copied/linked and token refresh written back. Rollouts/history live under `CODEX_HOME/sessions`, so **existing thread resume breaks** (M6.2 thread binding is out of scope for M7). User model-provider/profile settings are lost. Highest blast radius. |
| C | Rely on overlay replacement | Hope `config.mcp_servers` replaces the whole table instead of merging. | Unknown; current reading says no (merge per key). | Needs live probe; if it merges (expected), no effect. Not a real option unless the probe proves otherwise. |
| D | Detect, do not block | After the handshake call `mcpServerStatus/list` (already used as an outbound request) and the `mcpServer/startupStatus/updated` stream; compare against the allowlist; report the real server/tool set into the E2 capability snapshot and mark the session `mcp_policy = ambient_leak` when a non-allowlisted server is present. | No. Makes the leak visible. | No enforcement; the model can still call the tools. Low risk, no new codex surface. |
| E | Approval-gate MCP tool calls | Treat any MCP tool call from a non-allowlisted server as needing approval and auto-deny it in the approval handler. | Partially (calls, not tool advertisement). | Depends on codex emitting an approval request for MCP tool calls under the chosen `approvalPolicy`; with `never` (yolo) it does not. Model still sees the tools and wastes turns. Touches the permission path (PR 3 territory). |

## Conclusions

1. **Recommended path: D now, A next.** D needs no new codex behavior and feeds the
   E2 tool inventory with the *actual* server set, so C-08 can report "Codex: managed
   allowlist is a floor; ambient servers present" truthfully instead of claiming strict.
2. **A is the only enforcement option with a contained blast radius.** Implement it
   with codex as the source of truth for names (`mcpServerStatus/list` on the
   app-server *before* `thread/start`), not by parsing TOML ourselves, and pass the
   disables as process-level `-c` overrides so resume/fork/wake re-apply them
   (the same argv builder is shared by open and idle-wake).
3. **B is rejected for M7**: it breaks thread resume and touches auth, both of which
   are M6 (Codex auth / thread binding) territory that M7 must not touch.
4. **C and E are not worth pursuing** without a live probe proving C, and E overlaps
   the permission work in PR 3.

## Live probes needed before implementing A

1. On the pinned codex CLI: does `mcpServerStatus/list` answer before any
   `thread/start` and list ambient servers from `$CODEX_HOME/config.toml` and the
   project config?
2. Is `mcp_servers.<name>.enabled = false` honored both as `-c` on `app-server`
   and as a `thread/start` `config` overlay (server not launched, tools absent)?
3. Does an overlay entry for a name that also exists in the user config merge
   per-field (expected) or replace the entry?
4. Resume/fork: are `-c` process overrides re-applied after an idle-wake respawn
   (expected yes, shared argv builder) — confirm with `mcpServerStatus/list`.

## Risks common to every option

- MCP stays optional: with no allowlist and no Agent Center binding (`mcp_strict`
  absent), behavior must stay ambient, exactly as today.
- Older conversations carry no `mcp_strict` flag and keep ambient behavior.
- User-visible strings must say "CSBU WorkMate" only.
