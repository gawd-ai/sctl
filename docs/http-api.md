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
- [Errors](#errors)
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
   `tunnel_key` (`GET /api/tunnel/events` only as `Authorization: Bearer`,
   never from the query); `/d/{serial}/api/*` proxy routes authenticate with the
   **target device's own API key** as `Authorization: Bearer` (the relay
   checks it against the key the device presented at registration).

Malformed JSON bodies on JSON routes are rejected by the framework (axum)
with a `4xx` plain-text response before the handler runs; those rejections do
not carry the unified error shape below.

## Errors

Every handler-produced error body is the unified [`ApiError`]
(`server/src/error.rs`) shape:

```json
{
  "code": "SCREAMING_SNAKE",
  "message": "human-readable explanation",
  "detail": { "optional": "structured context" }
}
```

Match on `code`, never on `message`. Relay- and infra-produced bodies also
duplicate the message under a legacy `"error"` key until 0.7.0, and
relay transport-loss errors carry `"retryable": true`.

The full error model — the shape's field semantics, the `retryable`/retry
rules (including the exec-result check before re-running anything
side-effectful), and the complete code catalog with per-code meaning and
typical HTTP status — lives in [errors.md](errors.md). Per-endpoint
sections below list only the codes specific to that endpoint.

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
  "instance": "<32 hex characters>",
  "version": "0.6.12",
  "target": "mips_24kc",
  "layout": "gz-tmp",
  "cli": "/usr/bin/sctl",
  "upgrade": { "phase": "idle", "...": "..." },
  "sessions": 2,
  "tunnel": { "connected": true, "reconnects": 3, "...": "..." },
  "gps": { "status": "fix", "has_fix": true, "fix_age_secs": 2, "satellites": 9 },
  "lte": { "rssi_dbm": -67, "rsrp": -97, "sinr": 12, "signal_bars": 4, "band": "B7", "operator": "..." }
}
```

- `instance` (ADR-014): an opaque value made when this process started.
  It is an identifier, not a secret: random where the kernel gives
  randomness without waiting, mixed with the pid and the clocks, and never
  waited for. It is not a time and not a counter: two answers that carry
  the same value came from one process, and that is all it says. The helper of a
  [restart on request](#post-apirestart) tells the new process from the old
  one by it. It is not the `boot` that `net.state` and `infra.state` carry,
  which is the start time and stays behind the key.
- `target`, `layout` and `upgrade`: the compile target, the install layout
  (`null` without `install.json`) and the current `upgrade.state` body
  (`docs/upgrade.md`).
- `cli` (0.6.12): the path that runs the agent's tools on this unit,
  `/usr/bin/sctl` on every layout but the relay's (`/usr/local/bin/sctl`).
  On `gz-tmp` and `ramboot` the agent makes it at start: a guarded wrapper
  that runs its binary, through the loader on a unit that has one, only
  while root alone can change those files and their directories, and
  otherwise says why and exits 126; a symbolic link only for a binary root
  alone can change, every directory above it included (`docs/upgrade.md`,
  the tools' name). Absent when the agent has no `install.json` it can use
  or could not make the name, as when a regular file that is not its
  wrapper is there, or a `ramboot` unit runs its agent under a loader its
  `install.json` does not name; its log says why. `SCTL_CLI_PATH` moves the
  name for tests and benches.
- `tunnel` — in tunnel-client mode, extended with `uptime_secs`,
  `messages_sent/received`, `last_pong_age_ms`, `dropped_outbound`,
  `stream_backpressure_events`, `stream_replay_events`, `rtt_median_ms`,
  `rtt_p95_ms`, `recent_events` (last 10 tunnel events), `path` and
  `relay_route`. Otherwise only `{connected, reconnects}`.
- `tunnel.path`: where the tunnel's TCP connection runs,
  `{"local": "192.168.8.20:40022", "remote": "203.0.113.10:443", "dev": "eth1"}`.
  `dev` is the interface holding the local address (`null` for IPv6).
  The object is `null` while no TCP connection to the relay is open. It is
  set as soon as one opens, before registration, so it can be present while
  `connected` is still `false`. When the kernel's route to the relay moves
  to another address, the tunnel redials over it and records a `rehome`
  event such as `eth1 192.168.8.20 -> wwan0 10.64.3.2` (not with
  `bind_address`, which pins the tunnel).
- `tunnel.relay_route`: the host route sctl keeps to the relay
  (`[tunnel] relay_route` in the configuration reference),
  `{"mode": "follow_default", "dev": "eth1", "via": "10.42.0.1", "suspect": [], "rp_filter_strict": []}`.
  `mode` is `off`, `follow_default` or `prefer` (then `prefer` lists the uplinks
  in order); `dev` and `via` are `null` while sctl
  holds no route (always with `off`); `suspect` lists the uplinks that did
  not answer the relay and are waiting to be asked again. `rp_filter_strict`
  lists the uplinks sctl may probe whose reverse-path filtering is strict
  (`rp_filter` 1, from the larger of `conf/all` and `conf/<dev>`, read when
  the health is asked for): the answer to a probe is dropped there, so such
  an uplink cannot be proven. Always empty with `off`. Each move is a
  `relay_route` event such as `eth1 -> wwan0 (eth1: no answer from the relay)`.
- `gps` / `lte` — `null` when the subsystem is not configured;
  `{"status": "no_signal"}` / `{"status": "provider_unavailable"}` when
  configured but degraded.
- `network.verdicts` — on a unit whose failover engine (mwan3) tracks its
  uplinks, the engine's verdict per kernel interface, `{"eth1": "offline",
  "wwan0": "online"}`, the same `verdict` that `net.state` carries; absent
  otherwise.
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
| `groups` | string (CSV) | all | Any of `core`, `interfaces`, `disk`, `tunnel`, `gps`, `lte`, `infra`, or `all`. Unknown names are ignored. |

Response `200` — an object containing only the requested groups:

- `core` — `serial`, `hostname`, `kernel`, `system_uptime_secs`,
  `cpu_model`, `load_average`, `memory` (`total_bytes`, `available_bytes`,
  `used_bytes`), `safe_mode` (`{active, flag?}`).
- `interfaces` — array of network interfaces: `name`, `state`, `mac`, `addresses`
  (IP addresses unless `include_interface_addresses_in_info` is off), `master`
  (the bridge the interface is a port of, read from sysfs; `null` otherwise) and
  `bridge` (`true` when the interface is itself a bridge)
- `disk` — `disk` (root filesystem `total_bytes`/`used_bytes`, kept for
  back-compat) plus `disks` (all mounted storages).
- `tunnel` — `{connected, relay_url, reconnects}` (tunnel-client mode only;
  absent otherwise), plus `path` and `relay_route`, the same objects as in
  `/api/health`.
- `gps` — `status` plus, when a fix exists, `latitude`, `longitude`,
  `altitude`, `satellites`, `speed_kmh`, `course`, `hdop`, `fix_age_secs`
  (absent when GPS is not configured).
- `lte` — the provider's signal object plus `modem` identity and
  `detected_path` (absent when LTE is not configured).
- `infra`: the proof that the Infra monitor is checking (ADR-010), so a
  unit that pushes [`infra.state`](#infrastate-the-devices-infra-results)
  only on a change can stay quiet while its targets hold steady. Every
  agent from 0.6.12 answers it, a unit that holds no config included; an
  agent before 0.6.12 has no `infra` group, and its silence proves
  nothing. It is read from a cell of its own, never under the Infra lock,
  so a lock held for good shows as `rounds` standing still, never as a slow
  answer. It is not in `GET /api/health`, which answers without the key. A
  steady unit's group is about 60 bytes:

  ```json
  "infra": {"rounds": 17280, "seq": 42, "hash": "1666903c369dff7f"}
  ```

  - `rounds`: monitor rounds since the agent started. A round is counted at
    the end of a tick (every 5 s), and only while every running check is
    within 90 s plus one tick of taking its permit, so a monitor that is
    gone, blocked, stuck on a check or panicking in its tick moves no
    `rounds`. It never goes back while the agent runs. The same `rounds` at
    two polls, with no `off`: the monitor completed no round between them.
  - `seq`: 1 when the agent starts, +1 at every change it pushes: a
    target's status, a config applied or removed, a recovery run logged. A
    push or a removal that changes nothing is not a change.
  - `hash`: the first 16 hex characters, lowercase, of the SHA-256 of this
    text: the agent's `boot` in decimal, a newline, the config version in
    decimal (0 for none), a newline, then per target in byte order of its
    id: the id, a space, its status (`unknown`, `up`, `degraded` or
    `down`), a newline. While the agent runs it changes only when `seq`
    moves, and it can return to an earlier value. A release that hashes
    other text names the field anew. Test vector: the 61 bytes
    `1790000000000\n7\nSwitch down\napi-1 up\nt10 degraded\nt9 unknown\n`
    (each `\n` one newline) hash to `42189b104bcb1637`, and
    `1790000000000\n0\n`, a unit that holds nothing, to `c2ccc438d385f035`.
  - `off`: present only when no monitor is expected: `safe_mode` in an
    agent that started in safe mode, whatever the flag file says now, else
    `no_config` when the unit holds none. When it is absent a monitor is
    expected, and `rounds` moves every 5 s.

  `infra.state` and [`GET /api/infra/results`](#get-apiinfraresults) carry
  `boot`, `seq`, `hash` and `recoveries`, so a copy kept from either hashes
  by the same text. A change is pushed within about a second: a `hash` that
  differs from the copy's, or the same `hash` with a higher `seq`, that
  outlasts that second means the copy missed a report, and one pull of the
  results settles it. A matching `hash` also proves the copy's config
  version. A new `boot` in a report means the agent restarted: `rounds`
  and `seq` began again.

  For tests and benches, `SCTL_INFRA_TEST_STALL_SECS=<n>` in the agent's
  environment stops the monitor loop n seconds after it starts, with a
  warning in the log, so `rounds` stands still while the agent answers.

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

### `GET /api/net`

The device's network dumped from the kernel now, for a "recheck" that
cannot wait for the next change. **Auth: API key Bearer.** Reachable
through the relay like any device route.

Query parameters: none.

Response `200`: the `net.state` message the tunnel pushes (see
[`net.state`](#netstate-the-devices-network)), built from a fresh dump. `seq`
is `0`, since a fresh dump is outside the pushed sequence, and `boot` is the
agent's start as in the pushed messages.

Errors: `503` `NET_UNAVAILABLE` in relay mode (the relay does not watch its
network) or when the kernel's route socket cannot be opened.

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
| `env` | object&lt;string,string&gt; | no | Extra env vars merged into the inherited environment; `SCTL_AGENT_CONFIG` is the agent's own (`docs/config.md`) |
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
| `env` | object | no | Default env (per-command env merged over it, command wins); `SCTL_AGENT_CONFIG` is the agent's own (`docs/config.md`) |
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
`server.max_file_size` (default 50 MB) except `GET /api/files/raw`.

**The read gate** (ADR-011). `GET /api/files`, `GET /api/files/raw` and
`POST /api/stp/download` judge a file before they open it, and only then
open it, through the handle they judged:

- `403` `PROTECTED_PATH`: the agent's own keys and the copies the install,
  the upgrade and the WE826's move leave (`/etc/sctl`; `state_dir`;
  `data_dir` when it is also `state_dir`, else `<data_dir>/sessions`; the
  rollback directory `install.json` names; the config file the agent was
  started with; any file whose name begins with that file's name,
  `sctl.toml`, `infra-secrets.json` or `infra-monitor.json`); a process's
  secrets (`/proc/<pid>/environ`, `mem`, `cwd`, `fd`, `map_files`, also
  under `task/<tid>`); `/proc/kcore`, `/proc/kmsg`, `/proc/sysrq-trigger`;
  any procfs or sysfs file not on the agent's list; any debugfs or tracefs
  file. Every rule is matched on the path as given, then on the real path
  the links lead to; a directory or process rule matches its components
  anywhere in the path, so `/overlay/upper/etc/sctl/...` and
  `/proc/1/root/proc/self/environ` are refused too, and a name rule matches
  the file's name. The message names the rule, never the content.
- `400` `NOT_A_REGULAR_FILE`: a FIFO, a socket, a character or block
  device, never opened; on `files/raw` and in a transfer, also a listed
  procfs or sysfs file (the message names its filesystem).
- A file with no link left (deleted, or a memory file a process holds) is
  `404` `FILE_NOT_FOUND`.

The procfs and sysfs files a read returns, matched on the path as given:
`/proc/net/arp`, `/proc/net/route`, `/proc/net/dev`, `/proc/net/tcp`,
`/proc/net/tcp6`, `/proc/meminfo`, `/proc/loadavg`, `/proc/uptime`,
`/proc/cpuinfo`, `/proc/version`, `/proc/mounts`,
`/proc/sys/kernel/hostname`, `/sys/class/net/<if>/address` and
`/sys/class/net/<if>/operstate`. Each refusal is one `file_read` entry in
`GET /api/activity` whose summary says the read was refused and whose
`detail.rule` names the rule. Writes, deletes and listings are not gated.
The gate keeps routine reads away from secrets and keeps a read from harming
the unit; it is not an authorization boundary (`POST /api/exec` reads what
it refuses, recorded as exec).

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
truncated. `path` is the path as given.

A regular file is read until the size its stat gave (from `offset`, within
the cap) or its end. A listed procfs or sysfs file reports no size, so it is
read until it ends or the cap stops it: `size` is then the bytes to where
the read stopped, and `truncated: true` says the cap stopped it. The cap is
`max_file_size`, or `limit` below it.

Through the relay (`/d/{serial}/api/files`, and the legacy
`tunnel.file.read`) the cap is at most 4 MiB, half the tunnel's 8 MiB body,
so the answer fits even as base64. A larger file with no range is `400`
`FILE_TOO_LARGE` before it is read; ask for it by range. An answer that
would still not fit (text that JSON escapes heavily) is `FILE_TOO_LARGE`
too, never the tunnel's `502`.

Response `200` (directory): `{path, entries: [{name, type, size, mode?,
modified?, symlink_target?}]}` sorted by name; `type` is one of `file`,
`dir`, `symlink`, `other`.

Errors: `400` `INVALID_PATH` / `IS_DIRECTORY` / `FILE_TOO_LARGE` /
`NOT_A_REGULAR_FILE`, `403` `PROTECTED_PATH` / `PERMISSION_DENIED`, `404`
`FILE_NOT_FOUND`, `500` `IO_ERROR`.

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

Only a regular file on a filesystem that reports its size moves, so not a
procfs or sysfs file: a listed one is `NOT_A_REGULAR_FILE` (`GET /api/files`
reads it), and one off the list is `PROTECTED_PATH`. The body is exactly the
size its stat gave, with that `Content-Length`: a log that grows during the
download is cut where its size was read.

Errors: `400` `INVALID_PATH` / `IS_DIRECTORY` / `NOT_A_REGULAR_FILE`, `403`
`PROTECTED_PATH` / `PERMISSION_DENIED`, `404` `FILE_NOT_FOUND`, `500`
`IO_ERROR`.

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
[errors.md](errors.md#stp-transfer-codes) for the status mapping.

### `POST /api/stp/download`

Initialize a chunked download (device → client). **Auth: API key Bearer.**

Request body: `{path, chunk_size?}` (`chunk_size` in bytes; server default
when omitted). A `chunk_size` under 1024 is raised to 1024, as an upload's
is. One above 4 MiB, asked for or the server's default, is answered as
4 MiB, never refused, however large: 4 MiB is the most a file read
returns through the relay. The same holds for `gx.download.init` through
the tunnel. The answer gives the size used, and the client reads its
chunks by it.

Response `200`: `{transfer_id, file_size, file_hash, chunk_size,
total_chunks, filename}`.

The file passes the read gate first (see [Files](#files)): `403`
`PROTECTED_PATH`, and `400` `NOT_A_REGULAR_FILE` for a FIFO, a socket, a
device or a listed procfs or sysfs file (one off the list is
`PROTECTED_PATH`). The download then takes its place under
`max_concurrent_transfers` before it hashes the file (`429`
`MAX_TRANSFERS` past it), so at most that many hashes run, and the hash
reads exactly the size the stat gave. An empty file is one empty chunk.

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

A chunk is read only from the file the init hashed: the same device and
inode, still a regular file, its mtime unchanged. Anything else at the path
is `400` `FILE_CHANGED`, and is never opened. The transfer then fails: it
gives back its place under `max_concurrent_transfers`, and every later chunk
answers `FILE_CHANGED` too.

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

A session starts closed to AI clients (ADR-013): its `user_allows_ai` is
false unless the `session.start` that made it carries `user_allows_ai:
true`. A job (`job.start`) and a session brought back from its journal
after a restart start closed too. A person opens a session, or closes it,
with the WebSocket's `session.allow_ai` or this API's `PATCH
/api/sessions/{id}` `allowed`: the person's switch. The agent cannot tell
a person's client from an AI client on one key, so the clients sctl ships
send it only for a person: sctlin's Allow AI button, never mcp-sctl. The
flag gates the AI status, never a keystroke: `working: true` is refused
with `AI_NOT_ALLOWED` on a closed session, and an AI client that follows
the protocol, as mcp-sctl does, types only into a session whose AI status
the agent accepted.

### `GET /api/sessions`

List all sessions. **Auth: API key Bearer.**

Response `200`: `{sessions: [...]}` — each entry: `session_id`, `pid`,
`persistent`, `pty`, `kind`, `attached`, `status`, `idle`, `idle_timeout`,
`created_at`, `user_allows_ai`, `ai_is_working`, plus optional `exit_code`,
`name`, `ai_activity`, `ai_status_message`. `kind` is `terminal` or `job`.

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
| `allowed` | bool | The person's switch: open the session to AI clients or close it (broadcasts `session.ai_permission_changed`; closing clears any AI-working state) |
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

Push monitoring config: persists it to disk and runs the monitor loop for
it. **Auth: API key Bearer.**

Request body: an `InfraConfig` object — `version` (integer) and `targets`
(array of monitored targets, each with an `id` and a `check` spec).
`version` is the caller's name for the config, not a counter: the agent
reports it back in the results and the `infra` group's `hash`, and the
fleet sends a fingerprint of the config. It must not be 0, which means
the unit holds no config.

A push equal to the config the unit holds, every field and the version
included, changes nothing: the running monitor keeps running, no target is
checked again, nothing is pushed, and the config is written again only if
its last save failed. Any other config replaces the one held, and its
monitor replaces the running one. A recovery run in progress is not cut by
a push, a rebuilt monitor or a removal: it ends by itself or at its 300 s
limit. Outside safe mode a push starts a monitor whenever none runs for the
config held. In an agent that started in safe mode a push stores and saves
the config and starts nothing (`docs/safe-mode.md`). The monitor runs under
a supervisor, which rebuilds a loop that panicked after 5 s from the config
the unit then holds.

Each target's status follows its answers (ADR-010). A good answer above
`degraded_threshold_ms` is slow, and a slow answer never makes a target
Up:

| From | Good, within the threshold | Good, slow | Failure |
|---|---|---|---|
| `unknown` | `up` | `degraded` | `degraded` |
| `up` | `up` | `degraded` | `degraded` |
| `degraded` | `up` at `up_after_consecutive` such answers in a row | `degraded` | `down` at `down_after_consecutive` failures in a row |
| `down` | see below | see below | `down` |

From `down`, `up_after_consecutive` good answers in a row lead to `up` when
all were within the threshold, and to `degraded` otherwise; until then the
target stays `down`. A check that panics leaves its target `unknown`, its
detail saying the agent failed to check it.

A `recovery` (`enabled`, `command`, `cooldown_secs` with a default of 300,
`max_retries` with a default of 2, `credential_ids`) runs only on a failed
check of the monitor that leaves the target `down`: the first `down` and every failed check after it, within
the cooldown and `max_retries`, never on a good answer and never on Check
now. The `down` is signalled first, and the run never holds it back: it
leaves within about a second, whatever the run takes. The command can
start before that message leaves: when a recovery cuts the unit's own
uplink, the next connection pushes the picture as it then stands. The
command runs in a task of its own, outside the Infra lock, under a 300 s
limit, one run per target at a time, and the monitor checks the target
again only once the run has ended; Check now still reads it (below), and
the run's end applies the count and the cooldown. Before the command
starts, the run is saved in `<state_dir>/infra-recovery.json`, synced: the
count holds across restarts (after a start, a target with a saved count
waits a full cooldown), and the next start logs a run that did not end,
its outcome unknown. A run whose record did not reach the disk does not
run, since that record is what bounds a recovery that power-cycles the
unit's own supply across restarts: no command starts, the run is logged
with `exit_code` -1 and `ERROR: the run's record did not reach the disk;
the recovery did not run`, and it counts as an attempt for the count and
the cooldown, so `max_retries` still holds while the agent runs. The
count clears when a good answer moves the target out of `down` or
`unknown`; a removed config clears every count, and a target a push
removes loses its own. A managed upgrade waits for a run in progress, at
most 300 s, before it hands off (`docs/upgrade.md`).

Check kinds (`check.method`): `ping`, `http`, `https`, `tcp_port`, `snmp`,
`custom_script`, and `http_api`. An `http_api` check logs into a device's
JSON management API over sctl's own TLS stack (the same trust ladder as
`POST /api/fetch`: an explicit `pin_sha256` is authoritative) and reduces
what it says to a structured `data` object on the result. Fields:
`base_url` (`https://192.168.50.1`), `profile` (`peplink`), `pin_sha256`
(hex SHA-256 of the certificate DER; a check without one reports the
presented fingerprint as `presented_sha256` and fails rather than trusting
it), `credential_id` (a key into the credentials store, below), and
`timeout_ms` per request. The reduction runs on the device: for `peplink`
the client list leaves only as per-VLAN counts, and subscriber identifiers
(IMSI, ICCID, IMEI) are never carried.

An `snmp` check (its community) and an `http_api` check (its login) send a
stored secret, so they go only where the net tools' guard lets them, from a
socket bound there (ADR-012, the guide's "Network tools"): a unicast route
with no gateway, by an interface on the unit's LAN side, the LTE bearer
being the agent's `[lte] interface`. Off the LAN side the target reads down
with the guard's reason, as `SNMP NOT SENT (not_on_lan): ...` or
`API NOT SENT (not_on_lan): ...`, and nothing is sent. So does a target at
an address the unit holds on one of its interfaces, which the guard refuses
before any socket opens: `SNMP NOT SENT (own_address): <ip> is this unit's
own address: net tools send to devices on its LAN.` No detail quotes a
community. The `ping`, `http`, `https` and `tcp_port` checks send no secret
and are not guarded.

Response `200`: `{status: "ok", config_version, target_count, unchanged,
started}`. `unchanged` is true when the push equalled the config held;
`started` is true when the push started a monitor: always for another
config outside safe mode, for the same config only when none was running.

Errors: `404` `INFRA_UNAVAILABLE`, `400` `INVALID_REQUEST` (version 0).

### `DELETE /api/infra/config`

Stop monitoring, clear results and every recovery count, remove the
persisted config. A recovery run in progress goes on and is logged when it
ends. **Auth: API key Bearer.**

Response `200`: `{status: "ok", message: "Monitoring stopped"}`.

Errors: `404` `INFRA_UNAVAILABLE`.

### `GET /api/infra/results`

Latest monitoring results. **Auth: API key Bearer.**

Response `200`: `{boot, seq, hash, recoveries, ts, config_version,
targets: {target_id: result...}, recovery_log: [...]}`. `boot` (when the
agent started, unix ms), `seq`, `hash` and `recoveries` (recovery runs
logged since `boot`) name these results as `infra.state` names its
message (ADR-010). They are read with the results, so `hash` is the hash
of the picture these `targets` and this `config_version` show, and a
pull settles a comparison with the `infra` group of
[`GET /api/info`](#get-apiinfo). Each result carries `status`,
`latency_ms`, `since`, `consecutive_ok`, `consecutive_fail`,
`last_check`, `detail`, `name`, and, for `http_api` targets,
`http_status` and `data` (the profile's structured snapshot; after a
failed check the last good snapshot stays, with the status and counters
saying it is old). `consecutive_ok` counts good answers in a row, slow ones
included. Each `recovery_log` entry (the last 50 runs) is `{ts, target_id,
command, exit_code, stdout, stderr}`, `ts` being when the run was logged.
`stdout` and `stderr` are each masked (every password of at least 4 bytes
among the command's credentials becomes `***`, as written and in its HTTP
Basic form, the base64 of `username:password`, which a verbose HTTP client
writes to stderr; the longest first, so a password that holds another is
masked whole), then cut to 512 bytes on a character boundary, `...` marking
a cut. `exit_code` is -1 when the run gave none. Either a signal ended the
command (the out-of-memory killer, a crash, a kill from outside), and
`stdout` and `stderr` are then its own output; a shell that outlives a
command a signal ended exits 128 plus the signal's number instead. Or the
agent says why in `stdout`: `ERROR: credential <id> is not on this unit`,
`ERROR: the run's record did not reach the disk; the recovery did not
run` when the run was not saved before its command, which then never
started, `ERROR: spawn failed: ...` when the command could not start,
`ERROR: timeout` at the 300 s limit, another `ERROR:` when the agent could
not read the command's output or its end, `AGENT ERROR:` when the agent
failed to finish the run, or `OUTCOME UNKNOWN: the agent stopped before
this run ended (it started at <time>)` for a run the agent did not see
end, logged at its next start. A command's own output can begin with the
same words. When the subsystem is unavailable or holds no config this
returns an empty result set (`config_version: 0`), not an error. The same
per-target fields minus `data` are pushed over the tunnel as
[`infra.state`](#infrastate-the-devices-infra-results) whenever `seq`
moves, so a collector on the relay's event stream keeps a copy from the
pushes, compares it on each poll with the `infra` group of
`GET /api/info`, and pulls this route only when they disagree, or to read
the log entry of a new recovery run.

### `GET /api/infra/history/{target_id}`

Recent structured readings of one `http_api` target, newest last (ring of
30), so a collector that was cut off can backfill the window it missed.
**Auth: API key Bearer.**

Response `200`: `{target_id, samples: [{ts, status, latency_ms, data}]}`.

### `POST /api/infra/check/{target_id}`

Check now: an immediate reading of one configured target. No request body.
**Auth: API key Bearer.**

The check runs under the monitor's 90 s deadline, and its result is
applied as a monitor check's is: `last_check`, the latency, the detail, the
counters, the status and, for an `http_api` target, a data sample. A status
change is pushed like any other, so a few readings of a failing target
bring its `down` sooner. Check now counts no round and never starts a
recovery. A target has at most one check in flight: while one is, Check
now waits for that check, at most 90 s, and answers its result, and no
second check runs. A recovery run is not a check: while one of the target
runs, which the monitor leaves alone, Check now reads the target and its
result is applied as above, status included, but the target's recovery
count and cooldown stand until the run ends. The run's end then applies
them: a target that a good answer moved out of `down` or `unknown` during
the run, and that reads `up` or `degraded` when it ends, came back and
starts afresh, its count clear. Any other keeps the count and the cooldown
the run began: one that reads `down` or `unknown`, and one that never
answered, as a check that panicked (`unknown`) and then a failure leave
`degraded`. A result is applied only while the unit still holds the config
the check ran under and the target is in it; otherwise it is answered and
nothing is applied.

Response `200`: `{target_id, ok, latency_ms, detail, http_status, data,
presented_sha256}`. `presented_sha256` is set when a TLS target offered a
certificate other than the pinned one (or none was pinned): the fingerprint
an operator may now choose to pin. A session established by this call is
kept for the monitor.

Errors: `404` `INFRA_UNAVAILABLE` / `NOT_FOUND` (unknown target), `400`
`NOT_FOUND` (no config loaded).

### `POST /api/infra/credentials`

Store or replace one credential for `http_api` targets. **Auth: API key
Bearer.**

Request body: `{id, username, password}`. Credentials live in
`<data_dir>/infra-secrets.json` (owner-readable only), never in the
monitoring config, and no route ever returns a password. Replacing a
credential drops the cached login sessions of the targets that use it.

Response `200`: `{status: "ok", id, persisted}`.

Errors: `404` `INFRA_UNAVAILABLE`, `400` `INVALID_REQUEST`.

### `GET /api/infra/credentials`

Ids and usernames of stored credentials, for reconciliation. **Auth: API
key Bearer.**

Response `200`: `{credentials: [{id, username}]}`.

### `DELETE /api/infra/credentials/{id}`

Forget one credential. **Auth: API key Bearer.**

Response `200`: `{status: "ok", id, existed, persisted}`.

### `POST /api/infra/discover`

Run a LAN discovery scan (neighbour table, sweep, port probe, names) and
return the results when it ends, which can take minutes; poll
`GET /api/infra/discover/progress` from another request for live progress.
**Auth: API key Bearer.**

Request body: `{subnets?: ["192.168.1.0/24", ...]}`. With none, or an
empty list, the scan sweeps the networks the unit holds an address on, on
its LAN-side interfaces: the `addr` subnets of up `lan` interfaces in
[`GET /api/infra/discover/subnets`](#get-apiinfradiscoversubnets). Its LTE
and tunnel networks are never swept, and a `route` or `arp` subnet is swept
only when a request names it. An entry that is not an IPv4 network is not
swept, and `not_swept` names it as given: `not_ipv4` for an IPv6 address or
network, `invalid` for anything else not in `a.b.c.d/p` form (no prefix, a
prefix over 32, a word). Each entry is read on its own: one that is not a
string is `invalid`, named by its JSON text, and the other entries still
count. A `subnets` that is not a list is read as a list of that one entry.

A scan sends only on the unit's LAN side, as the net tools' guard judges it
(ADR-012; the guide's "Network tools"):

1. **Neighbours.** The kernel's neighbour table, read over netlink, gives
   the hosts on LAN-side interfaces: an entry holding a MAC the kernel
   would send to, at a unicast address. Their MACs are not taken from it.
2. **Sweep.** A subnet that lies wholly under a main-table route with no
   gateway through a LAN-side interface holding an address is swept on that
   interface only: `nmap -sn`, else `arp-scan` from the interface's
   address, else, for a subnet of at most 254 hosts, one ping to each host
   the guard passes on that interface, 32 at a time. The network and
   broadcast addresses and the unit's own are left out, and so is any
   address the kernel routes as broadcast or multicast, such as a network's
   broadcast address named as a /32. Each runs when the one before found
   nothing. Any other subnet (an LTE or tunnel network, one behind a
   gateway, one the unit has lost its address on) is not swept: `arp-scan`
   from 0.0.0.0, which stays on the link, searches it on each LAN-side
   interface, bridges first, until one finds hosts. arp-scan runs only on a
   subnet of at most 8190 hosts (a /19), which it can search within its
   60 s limit: a run the limit cuts keeps none of its answers. A LAN subnet
   of more than 254 hosts that neither nmap nor arp-scan swept is not swept
   either. The tools a unit lacks are skipped.
3. **Ports.** The guard judges each host. A host it approves gets a TCP
   connect to each of 22, 80, 443, 554, 8080, 8443, 161, 53, 3389 and 1337,
   2 s each, from a socket bound to the interface and source address it
   approved, at most 32 hosts at once; any other host gets none, the unit's
   own addresses included, which the guard refuses (`own_address`).
4. **Names.** A reverse lookup of each host through the unit's resolver,
   which may answer over the LTE (ADR-012 section 7).

A host's `mac` is the one MAC seen answering at its address during the
scan. Two things count as seen: an ARP answer the sweep received
(arp-scan's, or nmap's report of one), and the kernel's neighbour entry
for the host on the interface that reaches it, read once the port probe is
done, when it is REACHABLE or permanent then. A host with neither gets the
answer to one ARP request on that interface. An address two MACs answered
for gets none, whether the sweep heard both or the entry names another MAC
than the sweep's. A STALE, DELAY or PROBE entry is never reported and
never outweighs the sweep's answer: it may name the device that held the
address before. A host the guard keeps from the wire gets the sweep's MAC
or none. The unit's own addresses are never asked, by the entry or by ARP:
one carries a MAC only when another device answered the sweep there, as in
an address conflict.

Response `200`: `{ts, devices: [{ip, mac, hostname, open_ports,
inferred_type, mdns_services, own?}], scan_duration_ms, not_swept: [{cidr,
reason}]}`.

- `own`: present on a host that is the unit itself, naming the interface
  that holds its address, or else the one whose MAC answered for it (a
  bridge before its ports). The unit's own addresses in each subnet on the
  LAN side are listed without the sweep finding them, also in a subnet
  that is `too_large`.
- `not_swept`: each subnet the scan did not sweep, in network form, and
  why: `not_on_lan` (searched by ARP alone, as above) or `too_large`; and
  each entry of the request that is not an IPv4 network, once and as
  given, with `not_ipv4` or `invalid`.

Errors: `503` `NET_UNAVAILABLE` when the unit's network cannot be read:
the scan judges nothing, so it sends nothing.

### `GET /api/infra/discover/progress`

Current discovery scan progress. **Auth: API key Bearer.**

Response `200`: `{active, phase, phase_number, total_phases, hosts_found,
devices, started_at, elapsed_ms}`; `{active: false, phase: "idle"}` when no
scan is running. `phase` is `arp` (the neighbour table), `ping` (the sweep),
`ports`, `hostname`, then `complete`, or `error` when the unit's network
could not be read. A device's `mac` is the sweep's until the scan decides
it.

### `GET /api/infra/discover/subnets`

The unit's subnets, each with its role and how it was learned. The tunnel's
`tunnel.infra.discover.subnets` answers the same. **Auth: API key Bearer.**

Response `200`: `{subnets: [{iface, cidr, host_ip?, mac?, source,
role}]}`, `cidr` in network form and `mac` the interface's.

- `source`: `addr`, a network an interface holds, `host_ip` being the
  unit's address there; `route`, a /24 around a default route's gateway
  that no network of its interface holds; `arp`, a /24 a 5 s passive sniff
  of the LAN-side interfaces heard (only when `tcpdump` is installed).
- `role`: the interface's, by the net tools' guard (ADR-012 decision 2):
  `lte` for the agent's `[lte] interface` (with none the unit has, an
  interface named `wwan*`, `qmimux*` or `usb*`); `tunnel` for a
  point-to-point or no-ARP link (WireGuard, PPP, a raw-IP modem); else
  `lan`, a wired WAN port included.

The loopback, container bridges (`docker*`), veth pairs (`veth*`) and
legacy tunnels (`tun*`) are left out. A scan given no subnets sweeps the
`addr` subnets of up `lan` interfaces.

Errors: `503` `NET_UNAVAILABLE` when the unit's network cannot be read; the
body's `reason` is `ip_command_failed`, its name from before 0.6.12.

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

`tls` is absent for plain-http targets. `set_cookies` lists every `Set-Cookie` header in order (`headers` keeps one value per name); it is absent when there are none. Note the upstream status rides
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
A `session.start`'s `env` is merged as exec's is: `SCTL_AGENT_CONFIG` is
the agent's own (`docs/config.md`).

A `session.start` takes `user_allows_ai`, false when absent: only `true`
opens the session to AI clients at its start, for a person's choice
(ADR-013). Agents before 0.6.12 read an absent field as true. The
`session.started` answer carries the session's `user_allows_ai`, and a
`job.start` is answered `false`. `session.allow_ai` (`session_id`,
`allowed`) is the person's switch, answered `session.allow_ai.ack` and
broadcast as `session.ai_permission_changed`. `session.ai_status` with
`working: true` is refused with `AI_NOT_ALLOWED` on a closed session.
`session.listed` carries each session's `kind` (`terminal` or `job`) and
`user_allows_ai`, on this socket and through a relay
(`GET /d/{serial}/api/ws`), where agents before 0.6.12 leave `kind` out.

On disconnect, non-persistent sessions are killed; persistent sessions
detach and keep buffering output for later re-attach.

`session.attach` takes `session_id`, `since` and `dedupes`. It answers
`session.attached` with `entries`, the session's entries above `since`
(each a `session.stdout`, `session.stderr` or `session.system` message
with its `seq`), and `dropped`, how many entries after `since` the buffer
no longer holds; live output follows. On this socket each connection has
a subscriber of its own per session, which an attach restarts after its
replay, and `dedupes` changes nothing. Through a relay it does
(`GET /d/{serial}/api/ws`).

---

## Managed upgrades

The agent replaces itself with a signed release its relay serves, restarts,
proves the new agent healthy and rolls back on its own when it is not.
`docs/upgrade.md` is the contract; these are the device's three routes.
**Auth: API key Bearer.** Reachable through the relay like any device route,
which is how the fleet asks.

### `POST /api/upgrade`

Ask the device to upgrade. Body:

```json
{"version": "0.6.8.440",
 "request_id": "rollout-7f3a/ring-1",
 "manifest_url": "https://relay.example/api/tunnel/artifacts/0.6.8.440/release.json",
 "not_before": "2026-10-02T06:00:00Z",
 "not_after": "2026-10-02T09:00:00Z",
 "allow_downgrade": false,
 "mirror_base": "http://host/artifacts/0.6.8.440"}
```

`version` is required. `manifest_url` defaults to the device's own relay
(`https://<host of tunnel.url>/api/tunnel/artifacts/<version>/release.json`),
and on a relay (`[tunnel] relay = true`, no `url`) to its own artifacts route
on loopback (`http://127.0.0.1:<listen port>/api/tunnel/artifacts/<version>/release.json`);
the artifacts are fetched from the manifest's directory with the tunnel key.
`not_before` and `not_after` are the window the requester means: read
outside it, the request is refused and nothing happens. `mirror_base` is for
the ramboot layout only.

Responses: `202` `{accepted: true, state}` (staging started; the outcome
arrives as [`upgrade.state`](#upgradestate-where-the-devices-upgrade-stands)),
`200` `{accepted: false, state, running_version}` (already at that version).

Errors: `409` `UPGRADE_IN_FLIGHT` (one is in progress; `detail.state` says
which), `409` `RESTART_IN_FLIGHT` (a [restart on request](#post-apirestart)
is in flight; `detail.restart` is its record: the unit is busy, and the
upgrade is asked again once that record has ended), `423` `UPGRADE_HELD` (`[upgrade] hold` or `/etc/sctl/upgrade-hold`),
`422` `UPGRADE_REFUSED` with `detail.reason` one of `window_passed`,
`no_clock`, `downgrade`, `no_install_json`, `no_mirror`, `manifest_invalid`
(the version does not parse), `500` `UPGRADE_CONFIG` (a `trust_keys` entry
does not parse).

### `GET /api/upgrade`

Where the device stands. Response `200`: `{state, restart, running_version,
target, layout, install, install_error, hold}`. `state` is the
`upgrade.state` body (`phase`, `outcome`, `reason`, `from_version`,
`to_version`, `log_tail`, ...); `install` is `install.json` with the
layout's defaults applied; `layout` is `null` on a device installed before
0.6.7.

`restart` (ADR-014) is the record of the agent's last
[restart on request](#post-apirestart), `{"v": 1, "phase": "idle"}` when
none was ever asked. Its presence is how a caller knows the agent has the
route: an agent whose answer has no `restart` is never asked.

### `DELETE /api/upgrade`

Clear a finished state (for a person at the box; the fleet does not need
it). Response `200`: as `GET`. Error: `409` `UPGRADE_IN_FLIGHT`.

### `POST /api/restart`

Ask the agent to restart itself (ADR-014). The agent restarts as a managed
upgrade does, less everything an upgrade moves: a detached helper runs the
install's restart command and proves the process that comes back. A caller
sends no shell text and needs no knowledge of the unit's layout. The unit
does not reboot, its modem and its routes are not touched, and no
configuration changes. `docs/upgrade.md` has the whole of it.
**Auth: API key Bearer.** Reachable through the relay like any device route.

Body, both keys optional (an empty body is a request with neither):

```json
{"request_id": "r-41", "unless": ["sessions", "jobs", "transfers"]}
```

- `request_id`: the caller's name for this restart, at most 128 bytes,
  echoed in the record.
- `unless`: the kinds of work that refuse the restart while they run,
  decided by the agent at the instant it accepts: `sessions` (a terminal
  with a client attached), `jobs` (a running job), `transfers` (a file
  transfer in flight). Without it the restart is accepted and the answer
  says what it ends.

Response `202`, sent before anything stops:

```json
{"accepted": true,
 "restart": {"v": 1, "phase": "accepted", "request_id": "r-41",
             "boot_before": 1790000000000, "instance_before": "<32 hex characters>",
             "accepted_at": "2026-11-22T15:02:11Z"},
 "window_secs": 180,
 "second_run": false,
 "safe_mode": false,
 "ends": {"sessions": 2, "attached": 1, "jobs": ["pb:outlet-cycle"], "transfers": 0}}
```

- A caller behind a proxy that turns every success into `200` reads
  `accepted`, never the status.
- `ends`: what the restart ends. `sessions` counts the running terminals,
  `attached` those with a client attached, `jobs` names the running jobs
  (at most 16, each cut to 64 bytes), `transfers` counts the file
  transfers in flight. After the answer the agent ends every session and
  job itself, before its helper is started. A `POST /api/exec` in flight
  is not listed: the agent keeps no list of them.
- `window_secs`: how long the helper waits for the new process. On
  `gz-tmp`, `second_run` is true: when nothing answers at the end of the
  window (every health read of its last 30 s failed, at least three)
  the helper runs the restart command once more and waits one more window.
- `safe_mode`: whether the safe-mode flag is present now, in which case the
  next process starts in safe mode again. The route does not clear it.

Errors, each before anything stops. `409` `UPGRADE_IN_FLIGHT` (an upgrade
is staging or its helper has the unit, one started by `sctl upgrade` from a
shell included, by its helper's lock on disk; `detail.state`), `409`
`RESTART_IN_FLIGHT` (a restart is accepted or its helper is still
watching; `detail.restart` is its record), `400` `RESTART_BAD_REQUEST` (a
body that is not a request, a `request_id` over 128 bytes, an unknown word
in `unless`), `422` `RESTART_REFUSED` with `detail.reason` one of:

| `detail.reason` | When |
|---|---|
| `no_install_json` | The agent does not know how it is started. |
| `disabled` | The kill switch, `/etc/sctl/disabled`, exists: an agent stopped now would not start again. |
| `loader_unknown` | A `ramboot` unit runs its agent under a loader `install.json` does not name (`helper_prefix`): the helper cannot be started. |
| `start_unreadable` | A file the init needs to start is missing or unreadable: the agent's config, a `gz-tmp` payload, the ramboot wrapper or `ramboot.conf`. |
| `cache_invalid` | `ramboot`: a payload is not in the cache as `ramboot.conf` names it, so the start would download it over the uplink. |
| `no_space` | A layout that runs from `/tmp`: less free space there than the install record's `restart_tmp_kb` plus the size of the running binary, which the helper pins while the init writes the new copy. |
| `relay` | The agent is a relay. |
| `recovery_running` | An Infra recovery run is in progress. The route does not wait for it. From the accept to the stop no run starts: a Down is still recorded and pushed, and its run is left to the next process. |
| `sessions_open`, `jobs_running`, `transfers_running` | `unless` named that kind and it is running; `detail.ends` says what. |

**The record.** `GET /api/upgrade`'s `restart`, and the same key in every
[`upgrade.state`](#upgradestate-where-the-devices-upgrade-stands) push:

| Key | Meaning |
|---|---|
| `phase` | `idle`, `accepted` (answered; the agent is ending what the restart ends), `restarting` (the helper has the unit), `done`. |
| `outcome` | `ok`: a new process answered health twice. `not_restarted`: the last health read answered in the window was answered by the process that accepted the request, and it is not established that nothing answers. `not_healthy`: no new process answered twice within the window. `not_started`: the helper never ran. `interrupted`: the agent started again while the restart was in flight and its helper left no outcome, so by what it started is not known; `ok` is written only by the helper. |
| `detail` | One line for a person, beside `outcome`. |
| `attempts` | How many times the helper ran the restart command. 2 means the first run started nothing: a defect to chase, which the agent also logs at warn. |
| `request_id` | The caller's. |
| `boot_before`, `boot_after` | The `boot` of the process that accepted the request, and of the process the helper proved (written by that process). A caller ties the next `net.state` or `infra.state` to the request by them. |
| `instance_before`, `instance_after` | The same two processes by `GET /api/health`'s `instance`, which is what the helper reads. |
| `accepted_at`, `ended_at`, `log_tail` | When, and the last kibibyte of the helper's log. |

A key with nothing to say is left out.

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
  device ships them, with no relay change. The device marks the requests
  it dispatches this way, and `GET /api/files` returns at most 4 MiB to
  them (see [Files](#files)).
- **Streaming endpoints are excluded**: `/api/events` and `/api/ws` cannot
  ride a request/response frame; the passthrough refuses them with `404`
  `ROUTE_NOT_PROXIED`. Interactive streaming goes through the named
  `GET /d/{serial}/api/ws` route below.
- **Relay-added failure modes** (see [errors.md](errors.md#retryable-true)
  for `retryable` semantics): `502` `DEVICE_DISCONNECTED` /
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
"api_key": "<its API key>", "features": ["net.state", "infra.state"]}` and
receives `{"type": "tunnel.register.ack", "serial": "...", "features":
["net.state", "infra.state"]}`.
The registered key is what `/d/{serial}/api/*` clients must present.
Re-registration with the same serial evicts the stale connection (pending
requests drain as `DEVICE_RECONNECTING`); existing relay WS clients are
preserved across the reconnect. Devices missing heartbeats for
`heartbeat_timeout_secs` (default 45 s) are evicted.

`features` is optional on both sides, and a peer that predates it ignores
it. The relay keeps up to 16 of the device's entries of at most 32 bytes
each and drops anything else. A device sends `net.state` and `infra.state`
only when the ack advertises each.

#### `net.state`: the device's network

A device whose relay advertises `net.state` sends this text frame right
after the ack, then after every network change whose content differs from
the last one sent on that connection, at most one per second:

```json
{
  "type": "net.state", "v": 1,
  "boot": 1790000000000, "seq": 4, "ts": "2026-09-26T12:00:00Z",
  "interfaces": [
    {"name": "eth1", "operstate": "up", "carrier": true, "metric": 10, "ip": "10.42.0.7/24", "verdict": "offline"},
    {"name": "wwan0", "operstate": "unknown", "carrier": null, "metric": 40, "ip": "10.180.41.231/30", "verdict": "online"}
  ],
  "default_routes": [
    {"dev": "eth1", "via": "10.42.0.1", "metric": 10},
    {"dev": "wwan0", "via": null, "metric": 40}
  ],
  "relay_route": {"ip": "203.0.113.10", "dev": "eth1", "via": "10.42.0.1", "src": "10.42.0.7"},
  "tunnel": {"dev": "eth1", "local": "10.42.0.7", "remote": "203.0.113.10"},
  "wg_routes": [{"ip": "203.0.113.10", "dev": "eth1", "via": "10.42.0.1", "src": "10.42.0.7"}],
  "truncated": false
}
```

- `boot` is when the agent started (unix ms) and `seq` counts distinct
  network states since then, from 1; a new `boot` restarts `seq`. `ts` is
  the device's clock, for information only.
- `interfaces`: every interface but loopback, sorted by name. `operstate`
  is the sysfs word (`up`, `down`, `lowerlayerdown`, `dormant`, `testing`,
  `notpresent`, `unknown`); `carrier` is `null` on kernels that do not
  report it; `metric` is the lowest main-table IPv4 default-route metric on
  the interface; `ip` is its primary IPv4 address; `master` names the bridge
  (or bond) the interface is a port of and is absent when it is not enslaved.
  `verdict` is what the unit's own failover engine holds about the uplink
  (`online` or `offline`, from mwan3's `/var/run/mwan3/iface_state`, mapped
  to the kernel device through netifd) and is absent on a unit without one
  or on an interface it does not track. A wire with link, a lease and the
  lowest-metric default route can still read `offline`: the engine then
  steers the unit's traffic elsewhere, and the main table does not show it.
  The agent reports the verdict and interprets nothing; a reader that
  decides which uplink carries traffic takes the verdict over the routes.
- `default_routes`: main-table IPv4 default routes, lowest metric first.
- `relay_route`: the kernel's route to the relay the tunnel is connected to,
  what `ip route get` says (`null` when there is none). `tunnel`: the live
  tunnel socket's interface and addresses.
- `wg_routes`: the kernel's route to each IPv4 peer endpoint of `wg0`, read
  from the kernel as `wg show wg0 endpoints` reads them; empty without `wg0`.
- Caps: 32 interfaces (those with a default route first, then those with an
  address), 16 default routes, 8 WireGuard routes, 15-byte names.
  `truncated` is `true` when a cap left something out. The relay drops a
  `net.state` over 16 KiB.

The relay keeps the latest `net.state` of each connection (a new
connection starts with none), publishes it on
[`GET /api/tunnel/events`](#get-apitunnelevents), and forwards it to the
device's `GET /d/{serial}/api/ws` clients. `GET /api/net` answers with the
same message on demand.

#### `infra.state`: the device's Infra results

A device whose relay advertises `infra.state` sends this text frame right
after the ack, then whenever `seq` has moved since the last one sent on
that connection, at most one per second: a target's status changed, a
monitoring config was applied or removed, or a recovery run was logged. A
check that leaves the status alone (a latency reading, a counter) sends
nothing, and neither does a push or a removal that changes nothing. A
collector that reads the stream keeps the last message (or pull) as its
copy and checks it on each poll against the `infra` group of
[`GET /api/info`](#get-apiinfo), which proves that a quiet unit is still
checking; it pulls `GET /api/infra/results` only when they disagree.

```json
{
  "type": "infra.state", "v": 1,
  "boot": 1790000000000, "seq": 42, "hash": "1666903c369dff7f", "recoveries": 0,
  "ts": "2026-09-30T12:00:00Z",
  "config_version": 7,
  "targets": {
    "6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f": {
      "status": "up", "latency_ms": 40, "since": "2026-09-30T11:58:00Z",
      "consecutive_ok": 3, "consecutive_fail": 0,
      "last_check": "2026-09-30T12:00:00Z", "detail": "API OK",
      "name": "Peplink MAX BR1", "http_status": 200
    }
  },
  "truncated": false
}
```

- `boot` (when the agent started, unix ms, the `boot` of `net.state`),
  `seq`, `hash` and `recoveries` (recovery runs logged since `boot`), as
  they stood when the message was built. `seq` and `hash` are those of
  the `infra` group of [`GET /api/info`](#get-apiinfo), and
  `GET /api/infra/results` carries all four. `hash` covers every target,
  so a copy built from a message the cap cut does not match it, and a
  pull settles it. `recoveries` moved: a recovery ran, and its log entry
  is read once from `GET /api/infra/results`. The fields are additive and
  `v` stays 1; a message from an agent before 0.6.12 has none of them.
- `config_version` is the config the results reflect, `0` when the device
  holds none (then `targets` is empty). `ts` is the device's clock.
- `targets`, sorted by id: each entry carries what
  [`GET /api/infra/results`](#get-apiinfraresults) carries minus `data`:
  `status` (`unknown`, `up`, `degraded`, `down`), `latency_ms`, `since`,
  `consecutive_ok`, `consecutive_fail`, `last_check` (empty before the
  first check), `detail` (the first 160 characters), `name`, and
  `http_status` when the check speaks HTTP. No recovery log.
- Cap: the relay drops an `infra.state` over 16 KiB, so a message that
  would exceed it loses every `detail` first, then every `name`, then the
  tail of the target list (the first N by id are kept), and `truncated` is
  `true`.

The relay keeps the latest `infra.state` of each connection the way it
keeps `net.state`, publishes it on
[`GET /api/tunnel/events`](#get-apitunnelevents), and forwards it to the
device's `GET /d/{serial}/api/ws` clients.

### `GET /api/tunnel/devices`

List devices currently connected to the relay. **Auth: relay `tunnel_key`
via `?token=` query parameter** (`403` plain-text on mismatch).

Response `200`: `{devices: [{serial, clients, client_count,
last_heartbeat_ago_ms, pending_requests_count, session_subscriptions,
connected_since_ms, dropped_messages, last_gps_fix, last_lte_signal,
egress_ip, features, version, target, layout}]}`. `features` is what the
device offered at registration; `version`, `target` and `layout` are what a
0.6.7+ device said it is (`null` from an older agent).

### `GET /api/tunnel/events`

The relay's device event stream, a WebSocket of JSON text frames: which
devices are connected, what their network looks like and what their Infra
targets report, pushed as it changes. **Auth: relay `tunnel_key` as `Authorization: Bearer` only**; a
`?token=` query is never read. Failures before upgrade (plain text): `403`
on a missing or wrong key, `429` when 8 subscribers are already connected.

On connect the relay sends `hello`, then an `artifacts` frame (the release
bundles it holds, see below), then replays every connected device (its
`device.connected`, followed by its latest `net.state`, `infra.state` and
`upgrade.state`, those it has, in that order), then `replay.done`, then live
frames:

```json
{"type": "hello", "relay_version": "0.6.7.440", "relay_epoch": 1790000000000, "features": ["net.state", "infra.state", "upgrade.state"]}
{"type": "device.connected", "serial": "XE300-1", "connection_id": 17, "connected_at": 1790000123456, "egress_ip": "203.0.113.7", "features": ["net.state", "infra.state"], "replay": true}
{"type": "net.state", "serial": "XE300-1", "connection_id": 17, "received_at": 1790000123789, "state": {"type": "net.state", "v": 1, "...": "..."}, "replay": true}
{"type": "infra.state", "serial": "XE300-1", "connection_id": 17, "received_at": 1790000123801, "state": {"type": "infra.state", "v": 1, "config_version": 7, "...": "..."}, "replay": true}
{"type": "replay.done", "devices": 1}
{"type": "device.disconnected", "serial": "XE300-1", "connection_id": 17, "reason": "ws_close", "replay": false}
```

- `relay_epoch` is when the relay started, and `connected_at` and
  `received_at` are relay times, all unix ms. `egress_ip` is `null` when no
  forwarding header named it.
- Live frames carry `"replay": false`: `device.connected` when a device
  registers, `net.state` and `infra.state` when it sends one (`state` is
  the device's message as sent), and `device.disconnected` when a
  connection ends. `reason` is
  `ws_close`, `writer_failed`, `write_path_dead`, `heartbeat_timeout`,
  `send_failed` or `relay_shutdown`. A connection replaced by a newer one
  for the same serial gets no `device.disconnected`: the new
  `device.connected` supersedes it. No `net.state` or `infra.state` for a
  connection follows its `device.disconnected`.
- `serial` is always the one the relay registered, never a value from the
  device's payload.
- The replay can repeat a frame that also arrives live; deduplicate
  `net.state` and `infra.state` by
  `(relay_epoch, connection_id, state.boot, state.seq)`: connection ids
  start at 1 again when the relay restarts, so `relay_epoch` from the
  `hello` frame is part of the key. An `infra.state` from an agent before
  0.6.12 carries no `boot` or `seq`; deduplicate it by
  `(relay_epoch, connection_id, state.ts, state.config_version)`.
- A subscriber that falls more than 1024 frames behind gets
  `{"type": "resync"}` followed by a fresh replay and `replay.done`, and
  should rebuild its view from it.
- The relay pings every 20 s and drops a subscriber it has not heard from
  in 60 s, or that does not take a frame within 10 s.

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
`lte.watchdog`, `net.state`, `infra.state`) are relay-internal state feeds
and are not part of the stable client contract.

Every client watching a session is fed by one subscriber per session on
the device's tunnel connection, which sends output in batches: one
`session.stdout`, `session.stderr` or `session.system` message carries up
to 32 entries or 8 KiB, under the `seq` of its last entry.

- An attach without `dedupes` restarts that subscriber after its replay,
  as agents before 0.6.12 do for every attach. What the old subscriber had
  taken and not yet sent never reaches the clients already watching.
- `dedupes: true` (0.6.12) is for a client that writes an entry only when
  its seq is above the highest it holds for the session, and that listens
  for the session's output before it sends the attach. A subscriber
  already running is kept, and the replay ends where it stands: no batch
  holds entries on both sides of the replay's end, batches above it come
  after `session.attached`, and batches at or below it may come before or
  after it. Such a client holds live output until the reply, then writes
  the replay and the held output, each entry only when its seq is above
  the highest it wrote. With no subscriber running, the attach starts one
  after its replay, as without `dedupes`.

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

### Release bundles

The relay holds release bundles under `<data_dir>/artifacts/<version>/` and
serves them to its own devices (`docs/upgrade.md`, "Artifacts on the relay").
A bundle is accepted in order: `release.json` and `release.json.sig` first,
verified against the keys the agents embed (plus `[upgrade] trust_keys`),
then the files the manifest names, each checked against its SHA-256 as it
lands. Every change publishes `artifacts.changed` on
[`GET /api/tunnel/events`](#get-apitunnelevents), and every replay opens with
the same listing as `{"type": "artifacts", "versions": [...], "replay": true}`.

### `GET /api/tunnel/artifacts`

List the bundles. **Auth: `tunnel_key` or the relay's operator `api_key`, as
`Authorization: Bearer`.** Response `200`: `{enabled, versions: [{version,
verified, complete, channel, targets, published_at, min_from_version, notes,
files, missing, mirror}]}`, newest version first. `enabled` is `false` on a
relay with no `data_dir`.

### `PUT /api/tunnel/artifacts/{version}/{file}`

Store one file of a bundle; the body is the file. **Auth: operator
`api_key` as Bearer** (`403` otherwise). Response `200`: `{stored, bytes,
bundle}`.

Errors: `409` `MANIFEST_FIRST` (a file before a verified manifest), `422`
`MANIFEST_INVALID`, `VERSION_MISMATCH` (the manifest's version is not the
path's), `MANIFEST_BAD_SIGNATURE`, `MANIFEST_UNSIGNED`, `NOT_IN_MANIFEST`,
`SHA256_MISMATCH` (the file is removed), `413` `TOO_LARGE`, `503`
`NO_DATA_DIR`.

### `PUT /api/tunnel/artifacts/{version}/mirror`

Set where a ramboot device's boot fetcher (busybox curl, no modern TLS) can
fetch the same files over plain HTTP: body `{"url": "http://host/path"}`,
an empty `url` clears it. **Auth: operator `api_key`.** Response `200`:
`{version, mirror}`. Errors: `400` `INVALID_URL`, `404` `NOT_FOUND`.

### `GET /api/tunnel/artifacts/{version}/{file}`

Serve a bundle file. **Auth: `tunnel_key` (every device holds it) or the
operator `api_key`, as Bearer.** Response `200` with `Content-Length`,
`ETag` (the manifest's sha256), `Accept-Ranges: bytes`; a `Range: bytes=a-b`
request answers `206` with `Content-Range`, an unsatisfiable one `416`. A
download dropped on LTE resumes from where it stopped.

Errors: `404` `NOT_FOUND` (no such bundle or file).

### `DELETE /api/tunnel/artifacts/{version}`

Remove a bundle. **Auth: operator `api_key`.** Response `200`: `{removed}`.
Errors: `404` `NOT_FOUND`.
