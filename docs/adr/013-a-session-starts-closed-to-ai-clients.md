# ADR-013: A session starts closed to AI clients

Status: Accepted (2026-10-25). Realized by [TRD-9](../trd/TRD-9-release-0-6-12.md) (release 0.6.12), section 3.8. Cites `docs/http-api.md` (`session.start`, `session.allow_ai`, `session.ai_status`, `session.list`, `job.start`). Lines are at sctl `ece8b45`, and at `release-0.6.12` `88a81b5` where they differ.

## 1. Trigger

On 2026-10-20 an operator worked through the fleet's assistant on a site's monitoring for most of a day. The fleet asks a person before each consequential change on a card; the operator moved to a console session the assistant could type in, where nothing asked. Every session the console starts is open to AI clients by the agent's default, and the fleet's assistant typed there as root. The fleet will count a session open to its assistant only when a person opened it through the fleet (fleet ADR-0038), on every agent version. The agent's default is what every other client meets: an AI client holding a unit's key, sctl's own MCP among them, finds every session open.

## 2. What existed

- **Every session is created open.** `create_session_with_pty` and `create_job` share one insert that sets `user_allows_ai: true` (`server/src/sessions/mod.rs:367`).
- **An omitted field reads open** on both start paths: the direct socket (`server/src/ws/mod.rs:674`) and the tunnel (`server/src/tunnel/client.rs:3259`; `:3269` at `88a81b5`). Only an explicit `false` turns it off, after the spawn (`ws/mod.rs:710-716`; `tunnel/client.rs:3300-3305`, `:3310`).
- **A job is answered open** (`ws/mod.rs:850`; `tunnel/client.rs:3446`, `:3456`).
- **A session brought back after a restart is open again** (`sessions/mod.rs:809`). It is an archived buffer with no process, status exited (`server/src/sessions/session.rs:385-398`): what it exposes to a reader is its scrollback; nothing can be typed into it.
- **`session.allow_ai` is taken from any client on the key** (`ws/mod.rs:455-480`; `tunnel/client.rs:3760-3800`, `:3770`).
- **The flag gates the AI status, never a keystroke.** `session.ai_status` with `working=true` is refused when the flag is off (`sessions/mod.rs:636-650`); `session.exec` and `session.stdin` are taken from any client (`ws/mod.rs:335-364`; `tunnel/client.rs:3493-3545`). The flag holds only for a client that follows the protocol.
- **The tunnel's session list carries no `kind`.** It builds its answer by hand (`tunnel/client.rs:3685-3715`); the direct socket's list serializes `SessionListItem`, which has it (`ws/mod.rs:427-431`; `sessions/mod.rs:58-64`). A client that reaches a unit through a relay cannot tell a job from a terminal, except that jobs are non-PTY (`sessions/mod.rs:257`).
- **The MCP** starts every session open (`mcp/src/tools.rs:1013-1024`; `mcp/src/websocket.rs:181-191` sends the field only when false) and offers any AI client `session_allow_ai` (`tools.rs:553-575,701,1504-1530`), whose description asks AI agents not to call it; `session_ai_status`'s description points to it (`:577`). `session_exec`, `session_send` and `session_exec_wait` set the AI status, log its refusal and type anyway (`tools.rs:1070,1175,1417`; `websocket.rs:400-415`); `session_signal` sends a signal, Ctrl-C included, to any session (`tools.rs:397-418`). Its device tools act as root on a unit: `device_exec`, `device_exec_batch`, `device_file_write`, `device_file_delete` and `playbook_put` (`tools.rs:81,113,182,218,647`).
- **sctlin** sends no `user_allows_ai` on a start (`web/src/lib/types/terminal.types.ts:200-211`; `web/src/lib/utils/ws-client.ts:374-386`) and reads an absent answer as open (`web/src/lib/components/TerminalContainer.svelte:228,274`). Its Allow AI control is an unlabelled 20 px star at the right end of the bar under the terminal, grey when off, explained only by a title that holds a dash (`web/src/lib/components/ControlBar.svelte:45-63,172-191`).
- **The file tree inserts a file name as HTML** while a filter is typed (`web/src/lib/components/FileTree.svelte:178-186,305`): a file named with markup runs in the page of whoever filters that unit's tree.

## 3. Decision

1. **Every session starts closed to AI clients.** The insert's default is false; an omitted `user_allows_ai` is false on both start paths; a job starts and is answered false; a session brought back from its journal is false. An explicit `user_allows_ai: true` on a start is honoured: a person's client may ask for it.
2. **`session.allow_ai` is a person's frame.** It stays in the protocol, documented as the person's switch. The agent cannot tell a person's frame from an AI client's on one key, so this holds through the clients sctl ships, and an AI client that sends it breaks the protocol, not the agent.
3. **The MCP types only into a session a person opened to AI.**
   - `session_start` passes false.
   - `session_allow_ai`, its dispatch and its handler go.
   - `session_exec`, `session_exec_wait`, `session_send` and `session_signal` refuse a session whose flag is off, before anything is sent, naming how a person opens it (the Allow AI control in a console). The MCP follows the protocol the agent cannot enforce.
   - `session_kill`, `session_resize`, `session_read`, `session_list` and `session_attach` stay as they are: they type nothing.
   - `session_ai_status`'s description says a person opens a session to AI.
   - **The MCP stays a root channel.** Its device tools run as root on a unit for whoever runs it with the unit's key. This decision protects a person's console session from an AI client; it does not make the MCP safe to hand to one.
4. **sctlin starts closed and says so.** `WsSessionStartMsg` gains `user_allows_ai`; `startSession` sends it, false unless the embedder passes `userAllowsAi`; an absent answer reads closed. The control is a labelled button, "Allow AI", that shows whether it is on, with titles free of dashes.
5. **The tunnel's session list carries `kind`**, as the direct socket's does, so a client behind a relay tells a job from a terminal.
6. **The file tree escapes a name** before it marks a filter match.

## 4. Consequences

- A client that relied on the default starts closed: the person switches Allow AI on, and an AI client asks the person. The fleet's console will start sessions closed on every agent version (fleet ADR-0038), so the agent's default guards other clients.
- Sessions open when a unit upgrades keep their flag until switched off or ended; a session brought back after the upgrade's restart comes back closed. Nothing is turned off on the units.
- No route and no configuration field is added: the HTTP API and config docs gates pass unchanged. `docs/http-api.md` says `user_allows_ai` defaults to false, that `session.allow_ai` is the person's frame, and that the list carries `kind`.
- Staff work through the MCP opens a session to AI in a console first, or uses the device tools, which this decision does not change.
- It rides one release with ADR-010 to ADR-012, which costs a unit nothing beyond that release's set; it never ships alone, since each release costs every unit its agent set again (TRD-9 section 5).

## 5. Rejected alternatives

- **Gate keystrokes on the flag in the agent.** The flag is about AI clients; a person types on the same key with the flag off. Without telling a person's frame from an AI client's (device identity, per-caller keys: later records), the agent would block the person's own console.
- **Keep `session_allow_ai` with a stronger description.** A description asks; an AI client steered to call it calls it.
- **Leave the default and rely on the fleet.** Every other client, the MCP first, would keep finding every session open.
- **Close sessions already open at the upgrade.** It would cut a person's assistant out of a session they opened on purpose, mid-work; the person's switch is the consent, and it is theirs to turn off.

## 6. What TRD-9 proves

- `cargo test`, no root: a start without the field is closed on both paths; a job is started and answered closed; a journaled session comes back closed; an explicit true is honoured; the tunnel's list carries `kind`; the MCP's tool list holds no `session_allow_ai`, its start sends false, and its exec, exec-and-wait, send and signal into a closed session are refused with nothing sent.
- sctlin vitest: the start sends false unless the embedder asks; an absent answer reads closed; the button's label and state; a file named `<img src=x onerror=alert(1)>` renders as text with its match marked.
- On a bench agent behind a bench relay: a session started by a client that omits the field reads closed through the tunnel and directly; after a restart a journaled session reads closed; the MCP lists no `session_allow_ai`.

## 7. Settled questions

1. **Sessions open at the upgrade.** Settled: they keep their flag (section 4).
2. **`session_signal`.** Settled: it refuses a closed session, like typing: a Ctrl-C reaches the person's command.
3. **`session_kill`.** Settled: it stays. It types nothing, and the MCP's device tools already reach further.
