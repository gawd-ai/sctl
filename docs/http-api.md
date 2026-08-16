# sctl HTTP API

Complete reference for the HTTP/WebSocket surface exposed by `sctl serve`
(default listen: `0.0.0.0:1337`) and, in relay mode, the tunnel relay routes.
Covers sctl 0.6.0.

This file is gated by CI: `scripts/check-http-api-docs.py` cross-checks every
`.route()` registration in `server/src/main.rs` and `server/src/tunnel/relay.rs`
against the headings here. **Heading contract:** each route section starts with
a heading of exactly `` ### `METHOD /path` `` — one route (method + path pair)
per section, with the path spelled exactly as registered (including `{param}`
placeholders). Add a route without documenting it, or document a route that
does not exist, and CI fails.

## Contents

- [Authentication](#authentication)
- [Error shape](#error-shape)
- [Error codes](#error-codes)
- [Health and info](#health-and-info)
- [Exec](#exec)
- [Activity](#activity)
- [Files](#files)
- [STP file transfer](#stp-file-transfer)
- [Sessions](#sessions)
- [Shells](#shells)
- [Events](#events)
- [Playbooks](#playbooks)
- [GPS and LTE](#gps-and-lte)
- [Infra monitoring](#infra-monitoring)
- [Safe mode](#safe-mode)
- [Fetch](#fetch)
- [WebSocket](#websocket)
- [Tunnel relay](#tunnel-relay)

## Authentication

Four auth modes exist, depending on the surface:

1. **None** — `GET /api/health` only. Suitable for load-balancer probes.
2. **API key (Bearer)** — every other `/api/*` route on the device requires
   `Authorization: Bearer <api_key>` (the pre-shared key from `[auth] api_key`
   / `SCTL_API_KEY`). Failures: `401` `AUTH_MISSING_TOKEN` (header missing or
   malformed), `403` `AUTH_INVALID_TOKEN` (wrong key), `500`
   `SERVER_CONFIG_ERROR` (server misconfigured). Comparison is constant-time.
3. **API key (query param)** — `GET /api/ws` takes `?token=<api_key>` instead,
   because browsers cannot set headers on a WebSocket upgrade. Until 0.7.0
   this puts the key in the URL; the request trace span logs only the path,
   never the query.
4. **Tunnel key / device key (relay)** — see [Tunnel relay](#tunnel-relay).
   `/api/tunnel/*` admin routes authenticate with the relay's shared
   `tunnel_key`; `/d/{serial}/api/*` proxy routes authenticate with the
   **target device's own API key** as `Authorization: Bearer` (the relay
   checks it against the key the device presented at registration).

Malformed JSON bodies on JSON routes are rejected by the framework (axum)
with a `4xx` plain-text response before the handler runs; those rejections do
not carry the unified error shape below.

## Error shape

Every handler-produced error body is the unified [`ApiError`]
(`server/src/error.rs`) shape:

```json
{
  "code": "SCREAMING_SNAKE",
  "message": "human-readable explanation",
  "detail": { "optional": "structured context" }
}
```

- `code` — stable machine-readable identifier. Match on this, never on
  `message`.
- `message` — safe to display in UIs.
- `detail` — optional structured context (request inputs, `transfer_id`,
  `recoverable` flag, `request_id` echo, downstream errors). Omitted when
  there is nothing structured to say.
- **Legacy `error` duplicate** — error bodies produced by the tunnel relay
  and by the infra routes additionally duplicate the message under a legacy
  `"error"` key. It is redundant with `message` and will be removed in 0.7.0;
  do not write new code against it.

### `retryable: true`

Relay-originated error bodies carry `"retryable": true` when the transport
was lost **while the request was in flight** — the request may never have
reached the device, or may have completed on the device with the response
lost on the way back. This appears on:

- `DEVICE_DISCONNECTED` (`502`) — device tunnel dropped with the request
  pending.
- `DEVICE_RECONNECTING` (`502`) — device re-registered mid-request; the old
  connection's pending requests were drained.
- `TIMEOUT` (`504`, relay) — the device did not answer within the proxy
  timeout.

An error **without** `retryable: true` was a rejection, not a transport loss —
blind-retrying it will not help. Before retrying anything side-effectful
(exec in particular), check `GET /api/activity/{id}/result` — the device
caches completed exec results, so a "lost" command may in fact have run.

## Error codes

Device catalog (`server/src/error.rs::codes`), with the HTTP statuses routes
pair them with:

| Code | Typical status | Meaning |
|------|----------------|---------|
| `AUTH_MISSING_TOKEN` | 401 | Authorization header missing or malformed |
| `AUTH_INVALID_TOKEN` | 403 | Key present but wrong |
| `INVALID_REQUEST` | 400 | Bad parameter / body field |
| `INVALID_PATH` | 400 | Path not absolute, contains `..` or NUL |
| `INVALID_MODE` | 400 | Bad octal mode string |
| `INVALID_CONTENT` | 400/422 | base64 decode failed / unparsable playbook |
| `FILE_NOT_FOUND` | 404 | File or directory does not exist |
| `FILE_TOO_LARGE` | 400/413 | Exceeds `max_file_size` (or playbook 1 MB cap) |
| `IS_DIRECTORY` | 400 | Path is a directory where a file was expected |
| `NOT_A_DIRECTORY` | 400 | Upload target is not a directory |
| `NOT_FOUND` | 404 | Resource (activity result, playbook, subsystem) absent |
| `PERMISSION_DENIED` | 403 | OS permission error |
| `IO_ERROR` | 500 | Other I/O failure |
| `SESSION_NOT_FOUND` | 404 | No such session |
| `EXEC_FAILED` | 500 | Spawn or wait failure |
| `TIMEOUT` | 504 | Command / fetch / device response timed out |
| `BATCH_TOO_LARGE` | 400 | More commands than `max_batch_size` |
| `MULTIPART_ERROR` | 400 | Malformed multipart upload |
| `AI_NOT_ALLOWED` | 409 | AI status change on a session the user has not allowed |
| `MODEM_UNAVAILABLE` | 503 | Comms provider not available |
| `MODEM_AT_FAILED` | 500 | AT command failure (provider) |
| `TUNNEL_CONNECTED` | 409 | Band change/scan refused while tunnel is up (`force:true` overrides) |
| `SCAN_RUNNING` | 409 | A band scan is already in progress |
| `INVALID_URL` | 400 | Unparsable / non-http(s) fetch URL |
| `FETCH_FAILED` | 502 | Fetch connection/protocol failure |
| `CERT_PIN_MISMATCH` | 502 | Presented certificate differs from its pin — never retry through this |
| `CERT_UNTRUSTED` | 502 | No CA path, no pin, TOFU not requested |
| `SERVER_CONFIG_ERROR` | 500 | Server's own configuration broken |
| `TOO_MANY_CONNECTIONS` | 429 | SSE connection cap reached |
| `INFRA_UNAVAILABLE` | 404 | Infra subsystem not available on this device |
| `COMMS_CAPABILITY_UNSUPPORTED` | 501 | Active comms provider lacks the capability |

STP transfer errors reuse the shape with gawdxfer codes
(`TRANSFER_NOT_FOUND` 404, `HASH_MISMATCH` / `CHUNK_INTEGRITY` /
`FILE_CHANGED` 400, `DISK_FULL` 507, `MAX_TRANSFERS` 429, plus
`FILE_NOT_FOUND` / `PERMISSION_DENIED` / `FILE_TOO_LARGE` / `INVALID_PATH` /
`INVALID_REQUEST` as above) and put `{transfer_id, recoverable}` in `detail`.

Relay-originated codes: `DEVICE_NOT_FOUND` 404, `DEVICE_DISCONNECTED` 502,
`DEVICE_RECONNECTING` 502, `DEVICE_SEND_FAILED` 502, `DEVICE_QUEUE_STALLED`
502, `OVERLOADED` 503, `TIMEOUT` 504, `UNEXPECTED_BINARY` 500,
`DEVICE_RESPONSE_INVALID` 502, `DEVICE_DISPATCH_ERROR` (device's status),
`ROUTE_NOT_PROXIED` 404, `PAYLOAD_TOO_LARGE` 413, `INTERNAL` 500.

---

## Health and info

### `GET /api/health`

Liveness probe. **Auth: none.**

Query parameters: none.

Response `200`:

```json
{
  "status": "ok",
  "uptime_secs": 12345,
  "version": "0.6.0",
  "sessions": 2,
  "tunnel": { "connected": true, "reconnects": 3, "...": "..." },
  "gps": { "status": "fix", "has_fix": true, "fix_age_secs": 2, "satellites": 9 },
  "lte": { "rssi_dbm": -67, "rsrp": -97, "sinr": 12, "signal_bars": 4, "band": "B7", "operator": "..." }
}
```

- `tunnel` — in tunnel-client mode, extended with `uptime_secs`,
  `messages_sent/received`, `last_pong_age_ms`, `dropped_outbound`,
  `stream_backpressure_events`, `stream_replay_events`, `rtt_median_ms`,
  `rtt_p95_ms`, and `recent_events` (last 10 tunnel events). Otherwise only
  `{connected, reconnects}`.
- `gps` / `lte` — `null` when the subsystem is not configured;
  `{"status": "no_signal"}` / `{"status": "provider_unavailable"}` when
  configured but degraded.
- In relay mode, three extra fields appear: `connection_history` (recent
  device connect/disconnect records), `device_snapshots` (last-known
  telemetry per serial, survives disconnects), and `live_devices`
  (currently connected devices with heartbeat age, client counts, queue
  depth, `egress_ip`).

Errors: none — always `200`.

### `GET /api/info`

System information snapshot. **Auth: API key Bearer.**

Query parameters:

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `groups` | string (CSV) | all | Any of `core`, `interfaces`, `disk`, `tunnel`, `gps`, `lte`, or `all`. Unknown names are ignored. |

Response `200` — an object containing only the requested groups:

- `core` — `serial`, `hostname`, `kernel`, `system_uptime_secs`,
  `cpu_model`, `load_average`, `memory` (`total_bytes`, `available_bytes`,
  `used_bytes`), `safe_mode` (`{active, flag?}`).
- `interfaces` — array of network interfaces (with IP addresses unless
  `include_interface_addresses_in_info` is off).
- `disk` — `disk` (root filesystem `total_bytes`/`used_bytes`, kept for
  back-compat) plus `disks` (all mounted storages).
- `tunnel` — `{connected, relay_url, reconnects}` (tunnel-client mode only;
  absent otherwise).
- `gps` — `status` plus, when a fix exists, `latitude`, `longitude`,
  `altitude`, `satellites`, `speed_kmh`, `course`, `hdop`, `fix_age_secs`
  (absent when GPS is not configured).
- `lte` — the provider's signal object plus `modem` identity and
  `detected_path` (absent when LTE is not configured).

Errors: none in practice (missing subsystems simply omit their group).

### `GET /api/diagnostics`

On-demand troubleshooting snapshot: process health, system stats, network
state, recent service logs. **Auth: API key Bearer.**

Query parameters:

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `log_lines` | integer | 200 | Max log entries returned (capped at 1000) |
| `log_since` | string | `24h` | Log time range: `1h`, `6h`, `24h` |

Response `200`: `{process, system, network, logs, log_stats}` where
`process` = `{pid, rss_bytes, open_fds, threads, uptime_secs}`, `system` =
hostname/uptime/memory/load, `network` = interface state, `logs` = recent
service log entries, `log_stats` = counts by severity.

Errors: `500` (plain status) on internal failure.

---

## Exec

### `POST /api/exec`

Execute a single shell command (`<shell> -c <command>`). **Auth: API key
Bearer.**

Request body:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `command` | string | yes | Shell command string |
| `timeout_ms` | integer | no | Per-request timeout; default `server.exec_timeout_ms` |
| `request_id` | string | no | Opaque correlation ID echoed in the response |
| `working_dir` | string | no | Working directory override (`~` expanded) |
| `env` | object&lt;string,string&gt; | no | Extra env vars merged into the inherited environment |
| `shell` | string | no | Shell binary override (e.g. `/bin/bash`) |

Response `200`:

```json
{ "exit_code": 0, "stdout": "...", "stderr": "...", "duration_ms": 42, "request_id": "..." }
```

`stdout`/`stderr` are capped at 1 MB each; `request_id` is omitted if not
sent. Full results are cached and retrievable via
`GET /api/activity/{id}/result`.

Errors: `504` `TIMEOUT` (the `request_id` echo moves into `detail`), `500`
`EXEC_FAILED`.

### `POST /api/exec/batch`

Execute multiple commands sequentially. A failing command does **not** abort
the rest — its error is inlined in the results. **Auth: API key Bearer.**

Request body:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `commands` | array | yes | `[{command, timeout_ms?, working_dir?, env?, shell?}]` |
| `working_dir` | string | no | Default for all commands |
| `env` | object | no | Default env (per-command env merged over it, command wins) |
| `shell` | string | no | Default shell |
| `request_id` | string | no | Echoed in the response |

Response `200`: `{"results": [ExecResponse, ...], "request_id"?}` in command
order. Timeouts and spawn failures appear inline as `exit_code: -1` with the
error text in `stderr`.

Errors: `400` `INVALID_REQUEST` (empty `commands`), `400` `BATCH_TOO_LARGE`
(more than `server.max_batch_size`).

---

## Activity

Every exec, file operation, session event, and playbook operation is
journaled to an in-memory activity log (ring buffer,
`server.activity_log_max_entries`). Full exec outputs are additionally kept
in a bounded result cache.

### `GET /api/activity`

Read recent activity entries, newest-last, with optional filters. **Auth:
API key Bearer.**

Query parameters:

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `since_id` | integer | 0 | Return entries with `id > since_id` |
| `limit` | integer | 50 | Max entries (capped at 200) |
| `activity_type` | string | — | e.g. `exec`, `file_read`, `file_write`, `file_list`, `file_delete`, `session_start`, `session_exec`, `session_kill`, `session_signal` |
| `source` | string | — | e.g. `mcp`, `ws`, `rest` |
| `session_id` | string | — | Match `detail.session_id` |

Response `200`: `{entries: [{id, timestamp, activity_type, source, summary,
detail?, request_id?}]}` — for exec entries, `detail` carries `exit_code`,
`duration_ms`, output previews, and `has_full_output: true` when the full
result is cached.

Errors: none — unknown filter values simply match nothing.

### `GET /api/activity/{id}/result`

Retrieve the cached full exec result for an activity ID — the uncapped
`stdout`/`stderr` behind an `exec` entry's previews. This is also the
safe first retry after a relay `retryable: true` error on an exec. **Auth:
API key Bearer.**

Response `200`: `{activity_id, exit_code, stdout, stderr, duration_ms,
command, status, error_message?}` — `status` is `"ok"`, `"timeout"`, or
`"error"`.

Errors: `404` `NOT_FOUND` (result never existed or was evicted from the
cache).

---

## Files

All paths must be absolute, without `..` components or NUL bytes
(`400` `INVALID_PATH` otherwise). Reads/writes are capped at
`server.max_file_size` (default 2 MB) except `GET /api/files/raw`.

### `GET /api/files`

Read a file, or list a directory. **Auth: API key Bearer.**

Query parameters:

| Param | Type | Description |
|-------|------|-------------|
| `path` | string | Absolute path (required) |
| `list` | bool | List directory contents (also implied by a trailing `/`) |
| `offset` | integer | Byte offset for partial reads |
| `limit` | integer | Max bytes to read (capped at `max_file_size`) |

Response `200` (file): `{path, content, size, modified?, encoding?,
truncated?}` — `content` is UTF-8 text, or base64 with
`"encoding": "base64"` for binary; `truncated: true` when the file extends
beyond the returned bytes; `size` is the total file size. Without
`offset`/`limit`, files over `max_file_size` are rejected instead of
truncated.

Response `200` (directory): `{path, entries: [{name, type, size, mode?,
modified?, symlink_target?}]}` sorted by name; `type` is one of `file`,
`dir`, `symlink`, `other`.

Errors: `400` `INVALID_PATH` / `IS_DIRECTORY` / `FILE_TOO_LARGE`, `403`
`PERMISSION_DENIED`, `404` `FILE_NOT_FOUND`, `500` `IO_ERROR`.

### `PUT /api/files`

Write a file atomically (temp file + rename; readers never see partial
content — cross-filesystem targets will fail the rename). **Auth: API key
Bearer.**

Request body:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | yes | Absolute destination path |
| `content` | string | yes | UTF-8 text, or base64 when `encoding` is `"base64"` |
| `create_dirs` | bool | no | Create parent directories (default false) |
| `mode` | string | no | Octal permission string, e.g. `"0644"` |
| `encoding` | string | no | `"base64"` for binary content |

Response `200`: `{path, size, ok: true}`.

Errors: `400` `INVALID_PATH` / `INVALID_CONTENT` / `INVALID_MODE` /
`FILE_TOO_LARGE`, `403` `PERMISSION_DENIED`, `500` `IO_ERROR`.

### `DELETE /api/files`

Delete a file. **Auth: API key Bearer.**

Request body: `{"path": "/absolute/path"}`.

Response `200`: `{ok: true, path}`.

Errors: `400` `INVALID_PATH`, `403` `PERMISSION_DENIED`, `404`
`FILE_NOT_FOUND`, `500` `IO_ERROR`.

### `GET /api/files/raw`

Stream a file as raw bytes — no base64, no size cap. **Auth: API key
Bearer.**

Query parameters: `path` (absolute path, required).

Response `200`: `application/octet-stream` body with
`Content-Disposition: attachment` (RFC 6266/5987-safe filename) and
`Content-Length`.

Errors: `400` `INVALID_PATH` / `IS_DIRECTORY`, `403` `PERMISSION_DENIED`,
`404` `FILE_NOT_FOUND`, `500` `IO_ERROR`.

### `POST /api/files/upload`

Multipart file upload into a directory. Each file part is written atomically
(temp + rename). **Auth: API key Bearer.**

Query parameters: `path` (absolute target **directory**, required).

Request body: `multipart/form-data`; every part carrying a filename becomes
one file in the target directory. Filenames containing `/`, `\`, or equal to
`..` are rejected.

Response `200`: `{ok: true, files: [{path, size}]}`.

Errors: `400` `INVALID_PATH` / `NOT_A_DIRECTORY` / `FILE_TOO_LARGE` /
`MULTIPART_ERROR`, `403` `PERMISSION_DENIED`, `404` `FILE_NOT_FOUND`
(directory), `500` `IO_ERROR`.

---

## STP file transfer

Chunked, hash-verified, resumable transfers (gawdxfer). Chunk endpoints use
raw `application/octet-stream` bodies with `X-Gx-*` headers — no JSON
wrapping. All STP errors carry `detail: {transfer_id, recoverable}`; see
[Error codes](#error-codes) for the status mapping.

### `POST /api/stp/download`

Initialize a chunked download (device → client). **Auth: API key Bearer.**

Request body: `{path, chunk_size?}` (`chunk_size` in bytes; server default
when omitted).

Response `200`: `{transfer_id, file_size, file_hash, chunk_size,
total_chunks, filename}`.

### `POST /api/stp/upload`

Initialize a chunked upload (client → device). **Auth: API key Bearer.**

Request body: `{path, filename, file_size, file_hash?, chunk_size,
total_chunks, mode?}` — `file_hash` is the whole-file SHA-256; when empty
the server computes it after all chunks arrive. `mode` is an octal string.

Response `200`: `{transfer_id, chunk_size, total_chunks}`.

### `GET /api/stp/chunk/{xfer}/{idx}`

Serve one download chunk. **Auth: API key Bearer.**

Response `200`: binary chunk body (`application/octet-stream`) with headers
`X-Gx-Chunk-Hash` (SHA-256 of the chunk), `X-Gx-Chunk-Index`,
`X-Gx-Transfer-Id`, `Content-Length`.

### `POST /api/stp/chunk/{xfer}/{idx}`

Receive one upload chunk. **Auth: API key Bearer.**

Request: binary chunk body with required `X-Gx-Chunk-Hash` header (`400`
`INVALID_REQUEST` when missing).

Response `200` (ChunkAck): `{transfer_id, chunk_index, ok, error?}`.

### `POST /api/stp/resume/{xfer}`

Resume an interrupted transfer. **Auth: API key Bearer.**

Response `200`: `{transfer_id, direction, chunks_received, total_chunks,
chunk_size, file_size, file_hash}` — `chunks_received` lists indices already
held, so the client sends/fetches only the rest.

### `GET /api/stp/status/{xfer}`

Transfer status. **Auth: API key Bearer.**

Response `200`: `{transfer_id, direction, phase, filename, file_size,
chunks_done, total_chunks, bytes_transferred, elapsed_ms, error_count}`.

### `GET /api/stp/transfers`

List all transfers. **Auth: API key Bearer.**

Response `200`: `{transfers: [{transfer_id, direction, filename, file_size,
phase, chunks_done, total_chunks, bytes_transferred}]}`.

### `DELETE /api/stp/{xfer}`

Abort a transfer. **Auth: API key Bearer.**

Response `200`: `{ok: true, transfer_id}`.

---

## Sessions

Interactive shell sessions are **created** over the WebSocket
(`session.start`); the REST surface lists and manages them. All session
lifecycle changes are broadcast on the WS/SSE event streams.

### `GET /api/sessions`

List all sessions. **Auth: API key Bearer.**

Response `200`: `{sessions: [...]}` — each entry: `session_id`, `pid`,
`persistent`, `pty`, `kind`, `attached`, `status`, `idle`, `idle_timeout`,
`created_at`, `user_allows_ai`, `ai_is_working`, plus optional `exit_code`,
`name`, `ai_activity`, `ai_status_message`.

### `DELETE /api/sessions/{id}`

Kill a session and remove it. Broadcasts `session.destroyed`. **Auth: API
key Bearer.**

Response `200`: `{ok: true, session_id}`.

Errors: `404` `SESSION_NOT_FOUND`.

### `PATCH /api/sessions/{id}`

Combined update — rename, AI permission, AI status. Applies whichever fields
are present, in that order. **Auth: API key Bearer.**

Request body (all optional):

| Field | Type | Effect |
|-------|------|--------|
| `name` | string | Rename (broadcasts `session.renamed`) |
| `allowed` | bool | Set user's AI permission (broadcasts `session.ai_permission_changed`; revoking clears any AI-working state) |
| `working` | bool | Set AI working status (broadcasts `session.ai_status_changed`) |
| `activity` | string | AI activity label (with `working`) |
| `message` | string | AI status message (with `working`) |

Response `200`: `{ok: true, session_id}`.

Errors: `404` `SESSION_NOT_FOUND` (rename/permission), `409`
`AI_NOT_ALLOWED` (status change without user permission).

### `POST /api/sessions/{id}/signal`

Send a POSIX signal to a session's process. **Auth: API key Bearer.**

Request body: `{"signal": 15}` (integer signal number).

Response `200`: `{ok: true, session_id, signal}`.

Errors: `404` `SESSION_NOT_FOUND`.

---

## Shells

### `GET /api/shells`

List shells available on the device. **Auth: API key Bearer.**

Response `200`: `{shells: [...], default_shell}`.

---

## Events

### `GET /api/events`

Server-Sent Events stream. Subscribes to the same broadcast channel as WS
clients: session lifecycle (`session.started`, `session.destroyed`,
`session.renamed`, ...), AI status changes, activity (`activity.new`), and
transfer progress events all flow through. The SSE `event:` name is the
message's `type` field; the `data:` payload is the full JSON message.
Keep-alive comment every 15 s. **Auth: API key Bearer.**

Not proxied through the tunnel relay (a long-lived streaming response cannot
ride the REST-over-WS relay pattern) — the generic passthrough refuses it
with `ROUTE_NOT_PROXIED`.

Errors: `429` `TOO_MANY_CONNECTIONS` (64 concurrent SSE connections). A
slow consumer that falls behind receives an `error` event with
`{"code":"LAGGED","missed":N}` and keeps receiving from the current point.

---

## Playbooks

Playbooks are Markdown files with YAML frontmatter (`name`, `description`,
typed `params`) and a fenced `sh`/`bash` code block, stored in
`server.playbooks_dir`. Names must be non-empty, ≤ alphanumeric/`-`/`_`
only (`400` `INVALID_REQUEST` otherwise).

### `GET /api/playbooks`

List playbooks with summary info. Unparsable files are skipped with a
warning, not an error. **Auth: API key Bearer.**

Response `200`: `{playbooks: [{name, description, params: [names...]}]}`.

Errors: `500` `IO_ERROR`. A missing playbooks directory yields an empty
list, not an error.

### `GET /api/playbooks/{name}`

Full playbook detail. **Auth: API key Bearer.**

Response `200`: `{name, description, params: {name: {type, description,
default?, enum?}}, script, raw_content}`.

Errors: `404` `NOT_FOUND`, `422` `INVALID_CONTENT` (file exists but does not
parse), `500` `IO_ERROR`.

### `PUT /api/playbooks/{name}`

Create or update a playbook. The body is the raw Markdown document (not
JSON) and is validated (frontmatter + script block) before writing. **Auth:
API key Bearer.**

Response `200`: `{ok: true, name, path}`.

Errors: `400` `INVALID_CONTENT` (does not parse), `413` `FILE_TOO_LARGE`
(over 1 MB), `500` `IO_ERROR`.

### `DELETE /api/playbooks/{name}`

Delete a playbook. **Auth: API key Bearer.**

Response `200`: `{ok: true, name}`.

Errors: `404` `NOT_FOUND`, `500` `IO_ERROR`.

---

## GPS and LTE

These endpoints project state from the external comms provider (plugin). On
a device without the subsystem configured they return `404` `NOT_FOUND`;
with the subsystem configured but the provider down, `503`
`MODEM_UNAVAILABLE`; when the active provider lacks the needed capability,
`501` `COMMS_CAPABILITY_UNSUPPORTED`.

### `GET /api/gps`

Current GPS status, last fix, and history, as maintained by the provider
poller. **Auth: API key Bearer.**

Response `200`: the provider's GPS snapshot (`status`, `last_fix`
(`latitude`, `longitude`, `altitude`, `satellites`, `speed_kmh`, `course`,
`hdop`, ...), `fix_age_secs`, history). Shape is provider-defined.

Errors: `404` `NOT_FOUND` (GPS not configured), `503` `MODEM_UNAVAILABLE`.

### `GET /api/lte`

Current cellular link quality, modem identity, band history, and scan
status. **Auth: API key Bearer.**

Query parameters: `refresh=true` forces a fresh provider poll before
answering.

Response `200`: the provider's LTE snapshot (signal metrics, modem identity,
band info) with the server-side `watchdog` state overlaid.

Errors: `404` `NOT_FOUND`, `503` `MODEM_UNAVAILABLE`, `501`
`COMMS_CAPABILITY_UNSUPPORTED`.

### `POST /api/lte/bands`

Switch between locked and auto band modes. **Auth: API key Bearer.**

Request body:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `mode` | string | yes | `"locked"` or `"auto"` |
| `bands` | array&lt;integer&gt; | for `locked` | Band numbers, each 1–128 |
| `priority_band` | integer | no | Preferred band within the lock |
| `force` | bool | no | Override the tunnel-connected guard |

Response `200`: provider result (includes the refreshed `snapshot`).

Errors: `400` `INVALID_REQUEST` (bad mode / empty or invalid bands), `409`
`TUNNEL_CONNECTED` (refused while the tunnel is up, unless `force:true` —
a band change can drop the very link you are managing the device over),
`503` `MODEM_UNAVAILABLE`, `501` `COMMS_CAPABILITY_UNSUPPORTED`.

### `POST /api/lte/scan`

Start a background cellular band scan. **Auth: API key Bearer.**

Request body: `{bands?: [integers], include_speed_test?: bool, force?:
bool}` — bands default to a standard LTE band list.

Response `200`: provider scan-start result.

Errors: `409` `TUNNEL_CONNECTED` (unless `force:true`) or `SCAN_RUNNING`,
`503` `MODEM_UNAVAILABLE`, `501` `COMMS_CAPABILITY_UNSUPPORTED`.

### `POST /api/lte/speedtest`

Run a throughput test through the configured link interface, against
`[lte] speed_test_url` / `speed_test_upload_url`. No request body. Allows up
to 5 minutes. **Auth: API key Bearer.**

Response `200`: provider speed-test result.

Errors: `400` `INVALID_REQUEST` (no speed-test URL configured), `503`
`MODEM_UNAVAILABLE`.

### `POST /api/lte/usb_cycle`

Manually trigger the provider's modem recovery action (USB power cycle). No
request body. Allows up to 2 minutes. **Auth: API key Bearer.**

Response `200`: provider result.

Errors: `503` `MODEM_UNAVAILABLE`, `501` `COMMS_CAPABILITY_UNSUPPORTED`.

### `GET /api/lte/watchdog/history`

Rolling LTE-watchdog history (`watchdog_history.jsonl`). **Auth: API key
Bearer.**

Response `200`: JSON array of watchdog event entries; `[]` when the file
does not exist.

Errors: `500` `IO_ERROR`.

---

## Infra monitoring

LAN infrastructure monitoring (site targets, checks, discovery). Error
bodies from these routes still carry the legacy `error` duplicate alongside
`code` + `message`. When the subsystem is unavailable, mutating routes
return `404` `INFRA_UNAVAILABLE`.

### `POST /api/infra/config`

Push monitoring config: persists to disk, restarts the monitor loop with
the new config. **Auth: API key Bearer.**

Request body: an `InfraConfig` object — `version` (integer) and `targets`
(array of monitored targets, each with an `id` and a `check` spec).

Response `200`: `{status: "ok", config_version, target_count}`.

Errors: `404` `INFRA_UNAVAILABLE`.

### `DELETE /api/infra/config`

Stop monitoring, clear results, remove the persisted config. **Auth: API
key Bearer.**

Response `200`: `{status: "ok", message: "Monitoring stopped"}`.

Errors: `404` `INFRA_UNAVAILABLE`.

### `GET /api/infra/results`

Latest monitoring results. **Auth: API key Bearer.**

Response `200`: `{ts, config_version, targets: {target_id: result...},
recovery_log: [...]}`. When the subsystem is unavailable this returns an
empty result set (`config_version: 0`), not an error.

### `POST /api/infra/check/{target_id}`

Run an immediate on-demand check for one configured target. No request
body. **Auth: API key Bearer.**

Response `200`: `{target_id, ok, latency_ms, detail, http_status}`.

Errors: `404` `INFRA_UNAVAILABLE` / `NOT_FOUND` (unknown target), `400`
`NOT_FOUND` (no config loaded).

### `POST /api/infra/discover`

Run a LAN discovery scan (ARP, ping sweep, port probe, identification) and
return the results synchronously — can take on the order of minutes; poll
`GET /api/infra/discover/progress` from another request for live progress.
**Auth: API key Bearer.**

Request body: `{subnets?: ["192.168.1.0/24", ...]}` — auto-detected from
the routing table when omitted or empty.

Response `200`: discovery results (`devices` found with addresses, open
ports, and identification hints).

### `GET /api/infra/discover/progress`

Current discovery scan progress. **Auth: API key Bearer.**

Response `200`: `{active, phase, phase_number, total_phases, hosts_found,
devices, started_at, elapsed_ms}`; `{active: false, phase: "idle"}` when no
scan is running.

### `GET /api/infra/discover/subnets`

Auto-detected LAN subnets (from `ip` routing information). **Auth: API key
Bearer.**

Response `200`: `{subnets: [{cidr, ...}]}`.

Errors: `503` `EXEC_FAILED` (the `ip` command failed).

---

## Safe mode

The supervisor writes `<data_dir>/safe_mode.flag` when it detects a
crash-loop. While the flag exists, the next start skips every optional
subsystem (modem, GPS, LTE, watchdog, infra) and keeps only the management
plane (HTTP + tunnel + sessions) live.

### `GET /api/safe_mode/flag`

Inspect the flag. **Auth: API key Bearer.**

Response `200`: `{active: false}` or `{active: true, flag: {since_unix,
reason, consecutive_crashes}}`.

Errors: `500` (plain status) if the flag exists but cannot be read.

### `DELETE /api/safe_mode/flag`

Clear the flag. Idempotent — clearing an absent flag succeeds. Does **not**
restart the daemon; the operator restarts it (or lets the supervisor recycle
it) to bring the optional subsystems back. **Auth: API key Bearer.**

Response `200`: `{cleared: true}` or `{cleared: false, reason: "flag not
present"}`.

Errors: `500` (plain status) on filesystem failure.

---

## Fetch

### `POST /api/fetch`

Perform an HTTP(S) request **from the device**, using sctl's own TLS stack
(rustls + webpki-roots). Exists because the deployed hardware's own curl/
OpenSSL are too old to speak modern TLS; this is the same privilege as
`/api/exec`, lifted to where the modern TLS lives. **Auth: API key Bearer.**

Request body:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | — | Required. `http://` or `https://` only |
| `method` | string | `GET` | Any valid HTTP method |
| `headers` | object&lt;string,string&gt; | — | Extra request headers |
| `body` | string | — | Request body (base64 when `body_base64`) |
| `body_base64` | bool | false | Interpret `body` as base64 |
| `timeout_ms` | integer | 10000 | Capped at 120000 |
| `max_bytes` | integer | 1048576 | Response body cap, max 8 MiB (bounded read — an endless body is truncated, not buffered) |
| `pin_sha256` | string | — | Operator-supplied cert fingerprint (hex SHA-256 of DER). Authoritative: a mismatch is a hard error |
| `allow_tofu` | bool | false | Trust-on-first-use: record the presented cert if nothing else establishes trust |
| `http2` | bool | false | Force HTTP/2 (required for cleartext h2c, e.g. LAN gRPC) |

Trust is established in order: explicit `pin_sha256` → normal webpki CA
validation → previously recorded pin for this host:port → TOFU (only if
`allow_tofu`). The response always reports the certificate actually
presented so a pin can be confirmed out of band.

Response `200`:

```json
{
  "status": 200,
  "headers": { "...": "..." },
  "body": "...",
  "body_base64": false,
  "truncated": false,
  "elapsed_ms": 152,
  "http_version": "HTTP/1.1",
  "tls": { "sha256": "...", "trust": "ca|configured|tofu_matched|tofu_recorded", "cert_der_base64": "...", "alpn": "h2" }
}
```

`tls` is absent for plain-http targets. Note the upstream status rides
inside the JSON — the sctl response itself is `200` whenever the fetch
completed.

Errors: `400` `INVALID_URL` / `INVALID_REQUEST` (bad method) /
`INVALID_CONTENT` (bad base64 body), `504` `TIMEOUT`, `502`
`CERT_PIN_MISMATCH` (never retry through this — it is the one signal
distinguishing interception from an ordinary failure), `502`
`CERT_UNTRUSTED`, `502` `FETCH_FAILED`.

---

## WebSocket

### `GET /api/ws`

WebSocket upgrade for interactive shell sessions and live events. **Auth:
API key via `?token=<api_key>` query parameter** (browsers cannot set
headers on WS upgrades); `403` plain-text `Forbidden` on a bad token,
checked before the upgrade completes.

All messages are JSON objects with a `"type"` field; an optional
`"request_id"` on any client message is echoed on the corresponding
response(s). Client → server types: `ping`, `session.start`,
`session.exec`, `session.stdin`, `session.kill`, `session.signal`,
`session.attach`, `session.resize`, `session.list`, `session.allow_ai`,
`session.ai_status`, `shell.list`. Server → client types include `pong`,
`session.started`, `session.stdout`/`session.stderr`/`session.system`
(with `seq`), `session.exited`, `session.closed`, `session.attached`,
`session.listed`, `shell.listed`, and `error` (`{code, message,
session_id?}`). The full message table lives in `server/src/ws/mod.rs`.

On disconnect, non-persistent sessions are killed; persistent sessions
detach and keep buffering output for later re-attach.

---

## Tunnel relay

With `[tunnel] relay = true`, the same binary also acts as the fleet relay:
devices dial **out** to it over WS and register; clients then reach any
registered device **through** the relay without inbound connectivity to the
device.

### Relay access to device APIs: `/d/{serial}/api/*`

Every device route documented above is reachable through the relay at the
same path prefixed with `/d/{serial}` — e.g. `POST /d/{serial}/api/exec`,
`GET /d/{serial}/api/files?path=...`, `PUT /d/{serial}/api/playbooks/{name}`.

- **Auth: the target device's API key**, as `Authorization: Bearer
  <device_api_key>`. The relay validates it (constant-time) against the key
  the device presented at registration; the relay's `tunnel_key` grants no
  access here. Failures: `404` `DEVICE_NOT_FOUND` (device not connected),
  `401` `AUTH_MISSING_TOKEN`, `403` `AUTH_INVALID_TOKEN`.
- **Transport: generic passthrough.** The relay forwards the method, path,
  query string, selected headers (`authorization`, `content-type`,
  `accept`), and body (capped at 8 MiB → `413` `PAYLOAD_TOO_LARGE`) as an
  `http.request` frame over the device's tunnel; the device dispatches it
  into its own router, so behavior, parameters, and error bodies are the
  device route's own. New device endpoints are reachable the moment the
  device ships them — no relay change needed.
- **Streaming endpoints are excluded**: `/api/events` and `/api/ws` cannot
  ride a request/response frame; the passthrough refuses them with `404`
  `ROUTE_NOT_PROXIED`. Interactive streaming goes through the named
  `GET /d/{serial}/api/ws` route below.
- **Relay-added failure modes** (see [Error shape](#error-shape) for
  `retryable` semantics): `502` `DEVICE_DISCONNECTED` /
  `DEVICE_RECONNECTING` / `DEVICE_SEND_FAILED` / `DEVICE_QUEUE_STALLED` /
  `DEVICE_RESPONSE_INVALID`, `503` `OVERLOADED` (more than 256 pending
  requests for the device), `504` `TIMEOUT` (device did not answer within
  the proxy timeout, default `[tunnel] tunnel_proxy_timeout_secs` = 60 s),
  `DEVICE_DISPATCH_ERROR` (the device's dispatch layer refused the frame;
  status is the device's own).
- Only two device-proxy paths are **named routes** with relay-specific
  handling rather than the passthrough: `GET /d/{serial}/api/ws` (a
  streaming WebSocket upgrade cannot ride a request/response frame) and the
  binary STP chunk pair `GET|POST /d/{serial}/api/stp/chunk/{xfer}/{idx}`
  (binary tunnel frames, not JSON), documented below. Every other device
  endpoint rides the passthrough.

### `GET /api/tunnel/register`

Device-side WS registration endpoint — **devices** call this, not clients.
**Auth: relay `tunnel_key`**, as `Authorization: Bearer` (preferred) or
`?token=` query parameter (pre-0.6.0 payloads; removed in 0.7.0, taking
keys out of fronting-proxy access logs).

Query parameters: `serial` (required; 1–64 chars of `[A-Za-z0-9._-]`,
`400` plain-text on bad format). `403` plain-text on a bad tunnel key.

After the upgrade, the device sends `{"type": "tunnel.register",
"api_key": "<its API key>"}` and receives `{"type": "tunnel.register.ack",
"serial": "..."}`. The registered key is what `/d/{serial}/api/*` clients
must present. Re-registration with the same serial evicts the stale
connection (pending requests drain as `DEVICE_RECONNECTING`); existing
relay WS clients are preserved across the reconnect. Devices missing
heartbeats for `heartbeat_timeout_secs` (default 45 s) are evicted.

### `GET /api/tunnel/devices`

List devices currently connected to the relay. **Auth: relay `tunnel_key`
via `?token=` query parameter** (`403` plain-text on mismatch).

Response `200`: `{devices: [{serial, clients, client_count,
last_heartbeat_ago_ms, pending_requests_count, session_subscriptions,
connected_since_ms, dropped_messages, last_gps_fix, last_lte_signal,
egress_ip}]}`.

### `GET /d/{serial}/api/ws`

WS proxy to a device — the relay-side equivalent of the device's
`GET /api/ws`, multiplexed over the device's single tunnel connection.
**Auth: the device's API key via `?token=` query parameter.**

Failures before upgrade (plain text): `404` device not connected, `403`
invalid key, `429` too many clients for this device (max 32).

Speaks the same JSON message protocol as `GET /api/ws`. The relay tags
`request_id`s per client to route responses, tracks `session.attach`/
`session.kill` subscriptions to fan out `session.stdout`/`stderr`/`system`
only to watching clients, forwards lifecycle broadcasts to all of the
device's clients, and additionally delivers relay-only messages:
`tunnel.device_disconnected`, `tunnel.relay_shutdown`, and `session.gap`
(`{reason: "backpressure"}`) when output had to be dropped for a slow
client. Device telemetry broadcasts (`gps.fix`, `lte.signal`,
`lte.watchdog`) are relay-internal state feeds and are not part of the
stable client contract.

### `GET /d/{serial}/api/stp/chunk/{xfer}/{idx}`

Proxied STP download chunk. **Auth: the device's API key as `Authorization:
Bearer`.** Named (not passthrough) because the chunk crosses the tunnel as
a binary frame, not JSON.

Response `200`: binary chunk with `X-Gx-Chunk-Hash`, `X-Gx-Chunk-Index`,
`X-Gx-Transfer-Id` headers, exactly like the device route.

Errors: device STP errors pass through; additionally `502`
`DEVICE_RESPONSE_INVALID` if the device returns header-unsafe transfer
metadata, plus the standard relay failure modes.

### `POST /d/{serial}/api/stp/chunk/{xfer}/{idx}`

Proxied STP upload chunk. **Auth: the device's API key as `Authorization:
Bearer`.** Binary body with required `X-Gx-Chunk-Hash` header (`400`
`INVALID_REQUEST` when missing); the relay wraps it in a binary tunnel
frame.

Response `200` (ChunkAck): `{transfer_id, chunk_index, ok, error?}`.

Errors: device STP errors pass through, plus the standard relay failure
modes.
