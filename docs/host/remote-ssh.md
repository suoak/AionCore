# Headless Host over SSH (E14)

Brand: **CSBU WorkMate**. Transport uses **system OpenSSH** only (MIT/BSD).
No custom SSH server. No AGPL terminal stacks.

## Defaults (D14)

| ID | Rule |
|----|------|
| D14-1 | Headless binds **loopback** (`127.0.0.1` / `::1`) unless `--allow-non-loopback-bind` |
| D14-2 | Existing workspace auth required — `--local` / `--identity-mode local` are rejected with `--headless` |
| D14-3 | No password-in-argv; use SSH agent / keys / WorkMate token files (`0600`) |
| D14-4 | Plugin network allowlist unchanged |

## Start headless Host (remote machine)

```bash
aioncore --headless --host 127.0.0.1 --port 25808
# stdout includes: WORKMATE_HOST_READY mode=headless url=http://127.0.0.1:25808/health
```

Health check (local to the Host):

```bash
curl -fsS http://127.0.0.1:25808/health
```

## Desktop / client tunnel (system OpenSSH)

On the machine running WorkMate desktop/WebUI:

```bash
ssh -N -L 25808:127.0.0.1:25808 user@remote-host
```

Point the client at `http://127.0.0.1:25808` (same protocol as a local Host).
The tunnel is opaque transport; auth remains WorkMate workspace credentials.

Optional reverse tunnel (Host reaches client services) is out of scope for the
first PR — document only when needed.

## Resume cursors (E14)

Realtime envelopes carry an optional `cursor: { epoch, sequence }`
(`aionui_api_types::EventCursor`). The Host stamps every event-bus broadcast
and retains a bounded in-memory ring (default 512).

On reconnect the client sends:

```json
{ "name": "resume", "data": {}, "cursor": { "epoch": 1, "sequence": 42 } }
```

Semantics (`after_sequence`): the Host replays retained events with
`sequence > cursor.sequence` in the **same epoch**, then emits
`resume.complete` with `status` `ok` | `gap` | `unavailable` and the current
cursor. Replay uses the live-delivery rule: only events addressed to the
connection's user plus explicit global events (`aionui_realtime::visibility`).
`ok` means nothing in between was lost; **gap** means a new epoch (journal
lost or discarded), evicted / unjournaled events, or a client cursor ahead of
the Host — fall back to HTTP hydrate. The ring is persisted to
`{data_dir}/realtime` (bounded: 512 events, 32 MiB), so the epoch survives a
Host restart.

## Smoke

End-to-end (CI, Linux): `crates/aionui-app/tests/remote_host_ssh_e2e.rs` starts
the real `aioncore --headless`, a throwaway user-level `sshd` on a loopback high
port (config, keys and `authorized_keys` in a temp dir; no system SSH config)
and `ssh -N -L`, then checks: live events through the tunnel; drop tunnel →
publish → reconnect → `resume` `ok` with the missed event and no other user's
events; SIGKILL + restart Host → same epoch, `ok`, no reused sequence; lost
journal → `gap` with a new epoch.

```bash
WORKMATE_E14_SSH_REQUIRED=1 cargo test -p aionui-app --test remote_host_ssh_e2e -- --nocapture
# sshd not at its compiled-in prefix: WORKMATE_E14_SSHD=/path/sshd
#   WORKMATE_E14_SSHD_LIBEXEC=/dir/with/sshd-session+sshd-auth
```

Without sshd the test prints `SKIP` and passes unless `WORKMATE_E14_SSH_REQUIRED=1`
(CI sets it). `scripts/remote-host-ssh-smoke.sh` remains the manual policy smoke.

## Non-goals

- Vendoring or forking AGPL PTY/SSH servers
- Anonymous remote access
- Shipping a WorkMate-branded SSH daemon
