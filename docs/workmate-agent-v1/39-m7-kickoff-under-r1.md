# M7 Kickoff Under Risk R1

## Status

```text
M6 = OPEN
REAL CHATGPT ACCOUNT ACCEPTANCE = PENDING   (unchanged, see 36 / 37)
M7 = STARTED (E1/E2 only, under R1)
M6 REAL-ACCOUNT ACCEPTANCE = S1 EXIT CRITERION
```

## Decision

On 2026-10-22 (Asia/Taipei) the owner approved starting M7 before M6 closes, applying mitigation R1 of the competitive development plan:

> R1 — M6 real-device acceptance is delayed and blocks the M7 start. Mitigation: treat it as an S1 exit criterion and run it in parallel; E1/E2 do not depend on M6 and can go first.

This note supersedes only the sequencing sentence in [37-m6-implementation-report.md](https://github.com/suoak/AionUi/blob/main/docs/workmate-agent-v1/37-m6-implementation-report.md) ("M7 remains `NOT STARTED` until M6.1–M6.4 and M6 are closed by real-account evidence"). It does **not** close M6 or any M6.x gate: every `PENDING` in documents 32, 36 and 37 stays `PENDING` until real-account evidence is recorded there.

## Scope allowed before M6 closes

- **E1 — tool-surface consistency and security**: skill isolation (C-01/B-13), MCP allowlists (B-10, B-12), and the remaining B-09..B-15 items.
- **E2 — security wrap-up**: C-01..C-09.
- E3 / E4: design only, no code, during week 1.

Nothing in this scope touches Codex authentication, thread binding, model discovery, or usage (M6.1–M6.4).

## Exit criterion

S1 cannot be declared complete until the joint Windows real-account acceptance in [36-m6-packaged-validation.md](https://github.com/suoak/AionUi/blob/main/docs/workmate-agent-v1/36-m6-packaged-validation.md) passes and M6 is closed with evidence.

## First change

- AionCore PR "E1-a skill isolation + MCP allowlist" (C-01/B-13, B-12, B-10), Core only, no aionrs upgrade, no release.

## Location note

The M6 reports (documents 32, 36, 37) live in the CSBU WorkMate desktop repository (`suoak/AionUi`, `docs/workmate-agent-v1/`). This note is recorded alongside the first M7 AionCore change; it does not edit or replace those reports.
