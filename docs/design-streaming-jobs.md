# Design: Streaming jobs (A′)

**Status:** implemented 2026-05-26 (server + web). Builds via `rundev.sh build` (exit 0),
ts-rs bindings regenerated, `cargo clippy --release` clean, `svelte-check` 0 errors. **Not
yet runtime-validated end-to-end, not deployed** (device needs a riscv64 cross-build +
deploy, which awaits authorization).
**Decisions locked:** straight to the clean primitive (no PoC phase); a job is a distinct
`job.start` WS message backed by a `kind: Job` session; the device emits a typed
`session.exited` frame.

## Problem

Running a playbook (e.g. `speedtest-multi-eth`) from the web UI fails the moment a run
crosses ~30s with `Request timed out after 30000ms: /api/exec`. There are **three stacked
30s ceilings**, all default `30000`:

- Browser fetch — `web/src/lib/utils/rest-client.ts:4` (`DEFAULT_TIMEOUT_MS`), throws at `:59`. **This is the one that fires.**
- Device exec — `server/src/config.rs` `exec_timeout_ms` → `504 TIMEOUT` (`server/src/routes/exec.rs:106`).
- Relay proxy — `server/src/tunnel/relay.rs` body `timeout_ms` default `30_000` (~`:1865`).

Raising the numbers is not the goal. Long diagnostics, speed tests, and migrations should
stream output **as it happens** and have **no fixed wall-clock ceiling** while attached.

## Decision and the key correction

`POST /api/exec` is a blocking, buffered, one-shot call. For a relayed device it rides
`tunnel_request_json` — **one request, one reply, no streaming** (`server/src/tunnel/relay.rs:1483`).
That is precisely the path the ceiling lives on. **An HTTP `/api/jobs` route would ride the
same non-streaming path** and is therefore the wrong primitive.

The only streaming substrate that already works through the relay is the **WebSocket session
channel**: `session.stdout/stderr/system` frames, fanned out per-session by the relay
(`server/src/tunnel/relay.rs:1172-1193`).

> **A job is a one-shot, non-PTY session whose child process *is* the rendered script.**
> It streams over the existing `session.*` WS frames. Zero new tunnel protocol. No HTTP on
> the hot path.

## Why this is mostly wiring

| Need | Already exists |
|---|---|
| Spawn a process, stream stdout/stderr live | non-PTY session — `server/src/sessions/mod.rs:223` → `spawn_shell_pgroup` |
| Push frames to the browser as they arrive | `subscriber_task` → `entry_to_ws_message` → `session.stdout/stderr/system` (`server/src/ws/mod.rs:97`, `:123`) |
| Capture exit code | exit watcher pushes `"Process exited with code N"` + sets `Exited` (`server/src/sessions/session.rs:152-172`); `exit_code: Arc<Mutex<Option<i32>>>` (`:52`) |
| Survive a page reload mid-run | sessions journaled + replayable via `session.attach … since` (`web/src/lib/utils/ws-client.ts:389`) |
| Relay the whole thing | `server/src/tunnel/relay.rs:1172-1193` relays the session frame family; device handles `session.start` over the tunnel identically (`server/src/tunnel/client.rs:2361`) |
| Auto-cleanup when done | exited sessions reaped on next sweep (`server/src/sessions/mod.rs:785-807`) |

## Wire contract

Client → device (new):

```jsonc
{ "type": "job.start",
  "command": "<rendered script>",   // required
  "shell": "/bin/sh",               // optional, defaults to config default_shell
  "working_dir": "...",             // optional
  "env": { "K": "V" },              // optional
  "name": "pb:speedtest-multi-eth", // optional label
  "request_id": "..." }             // correlation, echoed in ack
```

Device → client:

- ack: reuse `session.started { session_id, pid, request_id, … }` (so existing subscriber
  wiring lights up unchanged).
- stream: existing `session.stdout` / `session.stderr` / `session.system` frames.
- completion: **new typed `session.exited { session_id, exit_code }`** — the wire slot is
  already reserved (`server/src/tunnel/relay.rs:1174` relays it; `web/src/lib/types/terminal.types.ts:315`
  already declares `WsSessionExitedMsg`, currently *synthesized* client-side). We make the
  device emit it for real, so completion does not depend on parsing the system line or
  winning a race with the reaper.

Jobs are non-PTY (clean stdout, no echo/ANSI — confirmed `server/src/sessions/mod.rs:223`).
The shell runs as `sh -c "<command>"` so the child runs the script and **exits on its own**;
its exit status is the job's exit code.

## Server implementation (ordered)

1. **`server/src/shell/process.rs`** — add `spawn_command_pgroup(shell, working_dir, command, env)`:
   a clone of `spawn_shell_pgroup` (`:48`) that adds `.arg("-c").arg(command)` and
   `stdin(Stdio::null())`. Keep the `setpgid(0,0)` `pre_exec` so signals/cancel still reach
   the whole process tree.
2. **`server/src/sessions/`** — add a `kind` to `SessionEntry` (`Terminal | Job`, default
   `Terminal`) and a `create_job(shell, working_dir, command, env, name)` path that builds the
   `ManagedSession` from the `spawn_command_pgroup` child via `ManagedSession::spawn`
   (reusing journaling + the exit watcher verbatim). Surface `kind` in `list_sessions`
   (`server/src/sessions/mod.rs:577`) so the UI can filter jobs out of the terminal list.
3. **`server/src/sessions/session.rs`** exit watcher (`:152-173`) — after pushing the System
   line, broadcast a typed `session.exited { session_id, exit_code }` via
   `state.session_events`. (Thread the broadcaster in, or emit from the manager when it
   observes the status flip — prefer the watcher for promptness.)
4. **`server/src/ws/messages.rs`** — add the `SessionExited { session_id, exit_code }`
   variant to `WsServerMsg` (tag `session.exited`).
5. **`server/src/ws/mod.rs`** — add a `"job.start"` arm mirroring the `"session.start"` arm
   (`:238`): parse `command`/`shell`/`working_dir`/`env`/`name`, call `create_job`, send the
   `session.started` ack, spawn the same `subscriber_task` (`:281`).
6. **`server/src/tunnel/client.rs`** — add the matching `"job.start"` arm next to
   `"session.start"` (`:2361`) so jobs work over the relay tunnel.
7. **Relay** — no change. Verified: `session.*` frames + `session.exited` already relayed
   (`server/src/tunnel/relay.rs:1172-1193`).

## Web implementation (ordered)

1. **`web/src/lib/utils/ws-client.ts`** — add `startJob(opts: { command, shell?, workingDir?, env?, name? })`
   returning the `session.started` ack (mirror `startSession`, `:374`). The `onOutput`
   (`:296`), `onSessionGap` (`:312`), `attachSession` (`:389`), `killSession` (`:398`),
   `sendSignal` (`:428`) helpers all work as-is. Drop the synthetic `session.exited` shim in
   favor of the now-real frame (`terminal.types.ts:315,610`).
2. **`web/src/lib/components/PlaybookExecutor.svelte`** — replace `restClient.exec(script)`
   (`:60`, the blocking call) with: `startJob({ command: script, name: 'pb:'+playbook.name })`
   → subscribe `onOutput` → append frames into a **live** output pane (today the Result box
   only renders post-completion — this becomes a streaming log). On `session.exited`: show
   exit code, mark done; keep the existing "view full output" → ExecViewer path
   (`:198-222`). Add a Cancel button → `killSession` / `sendSignal`.
3. **Wiring** — `PlaybookExecutor` currently only gets `restClient`. The widget path can
   build a `SctlWsClient` (it already holds `wsUrl`+`apiKey` — `web/src/lib/widgets/PlaybookWidget.svelte:41`);
   the panel path (`web/src/lib/components/PlaybookPanel.svelte:149`) needs one prop threaded
   down. Prefer passing the already-shared `SctlWsClient` instance rather than constructing a
   second socket.
4. **Terminal UI** — filter `kind: Job` sessions out of the terminal tab list so jobs don't
   masquerade as interactive terminals.

## Edge semantics

- **Reaper race** — exited sessions are removed on the next sweep (`server/src/sessions/mod.rs:785`).
  The System exit line is pushed *before* the status flip, and the `subscriber_task` holds the
  buffer `Arc`, so it drains regardless. The typed `session.exited` broadcast removes any
  dependence on winning that race.
- **Output cap / backpressure** — `OutputBuffer` is a ring; a firehose job rolls old lines off
  and emits `session.gap`. Acceptable for playbooks (the summary table is the payload); the UI
  already understands gaps (`onSessionGap`).
- **Cancel** — reuse `session.kill` / `session.signal` (already wired). Free cancel button.
- **Runaway job** — `idle_timeout` only kills *detached idle* sessions, so an attached job has
  no 30s ceiling (the goal) and no hard cap. Optional `max_runtime_ms` later; cancel covers it
  for now.
- **Auth parity** — `job.start` rides the same authenticated WS as `session.start`; no new
  surface.

## Build / verify / deploy

- Build the device server via `rundev.sh` (never `cargo`/`cross` directly).
- Validate: start a job that runs `speedtest-multi-eth` end-to-end (>30s), confirm frames
  stream live and `session.exited` carries the right code; confirm it works **through the
  relay** (the whole point), not just direct.
- This needs a device sctl rebuild + redeploy before it is demoable. **Do not deploy unless
  explicitly instructed.**

## Out of scope (noted for later)

- **`POST /api/jobs` HTTP route** — would still be poll-based (cannot stream through the relay
  without net-new tunnel protocol). The MCP already exposes `session_start` /
  `session_exec_wait` / `session_read` for agent-driven long runs, so no HTTP job route is
  needed for the streaming UX.
- **Job listing / history surface** — beyond filtering `kind: Job` out of the terminal tab.
- **`max_runtime_ms` hard cap.**

## Grounding index

- 30s ceilings: `web/src/lib/utils/rest-client.ts:4,59`; `server/src/routes/exec.rs:106`; `server/src/tunnel/relay.rs:~1865`
- one-shot HTTP path (no stream): `server/src/tunnel/relay.rs:1483`
- streaming substrate: `server/src/ws/mod.rs:97` (`entry_to_ws_message`), `:123` (`subscriber_task`), `:238` (`session.start` arm), `:281` (subscriber spawn)
- session spawn / lifecycle: `server/src/sessions/mod.rs:223` (pipe session), `:577` (`list_sessions`), `:785-807` (reaper); `server/src/sessions/session.rs:52` (`exit_code`), `:152-173` (exit watcher); `server/src/shell/process.rs:48` (`spawn_shell_pgroup`)
- relay fan-out (jobs inherit it): `server/src/tunnel/relay.rs:1172-1193`; device tunnel handler `server/src/tunnel/client.rs:2361`
- web client: `web/src/lib/utils/ws-client.ts:296,312,374,389,398,428`; `web/src/lib/components/PlaybookExecutor.svelte:60`; `web/src/lib/widgets/PlaybookWidget.svelte:41`; `web/src/lib/components/PlaybookPanel.svelte:149`; `web/src/lib/types/terminal.types.ts:315,610`
