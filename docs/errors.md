# sctl error model

How errors leave the sctl HTTP surface: the wire shape, the retry semantics,
and the complete code catalog. Covers sctl 0.6.0. Endpoint-by-endpoint
behavior lives in [http-api.md](http-api.md); this file is the error
reference both device routes and the tunnel relay share.

## Contents

- [The unified shape](#the-unified-shape)
- [Legacy `error` duplicate](#legacy-error-duplicate)
- [`retryable: true`](#retryable-true)
- [Device code catalog](#device-code-catalog)
- [Comms provider codes](#comms-provider-codes)
- [STP transfer codes](#stp-transfer-codes)
- [Net tool refusals](#net-tool-refusals)
- [Restart refusals](#restart-refusals)
- [Relay-only codes](#relay-only-codes)

## The unified shape

Every handler-produced error body is the unified `ApiError` shape
(`server/src/error.rs`):

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

The shape is exported to the web client as a typed `ApiError` definition via
`ts-rs` (`cargo test export_bindings`).

Two caveats at the edges:

- **Framework rejections** — malformed JSON bodies on JSON routes are
  rejected by axum with a `4xx` plain-text response before the handler runs;
  those do not carry the unified shape.
- **Relay auth/registration plain-text** — a handful of relay upgrade-path
  rejections (invalid tunnel key on `/api/tunnel/*`, invalid serial format,
  wrong device key on the named `/d/{serial}/api/ws` route) are plain-text
  status responses, not JSON.

## Legacy `error` duplicate

Error bodies produced by the tunnel relay and by the infra routes
additionally duplicate the message under a legacy `"error"` key:

```json
{ "error": "same text", "code": "...", "message": "same text" }
```

It is redundant with `message` and will be removed in 0.7.0. Do not write
new code against it.

## `retryable: true`

Relay-originated error bodies carry `"retryable": true` when the transport
was lost **while the request was in flight** — the request may never have
reached the device, or may have completed on the device with the response
lost on the way back. Exactly three codes carry it:

| Code | Status | When |
|------|--------|------|
| `DEVICE_DISCONNECTED` | 502 | Device tunnel dropped with the request pending; all pending requests are drained with this error |
| `DEVICE_RECONNECTING` | 502 | Device re-registered mid-request; the stale connection's pending requests were drained |
| `TIMEOUT` (relay) | 504 | Device did not answer within the proxy timeout (`[tunnel] tunnel_proxy_timeout_secs`, default 60 s) |

An error **without** `retryable: true` was a rejection, not a transport
loss — blind-retrying it will not help.

**Exec is special.** The device caches completed exec results, so a "lost"
command may in fact have run. Before re-running anything side-effectful,
check `GET /api/activity/{id}/result` (reachable through the relay
passthrough as `/d/{serial}/api/activity/{id}/result`) — if the result is
there, the command completed and only the response was lost.

## Device code catalog

The catalog lives in `server/src/error.rs::codes`. "Typical status" is what
the route call sites pair the code with; a few codes appear with more than
one status depending on the route.

| Code | Typical status | Meaning |
|------|----------------|---------|
| `AUTH_MISSING_TOKEN` | 401 | `Authorization` header missing or malformed |
| `AUTH_INVALID_TOKEN` | 403 | Key present but wrong (constant-time comparison) |
| `INVALID_REQUEST` | 400 | Bad parameter or body field (empty command list, bad band mode, missing required header, …) |
| `INVALID_PATH` | 400 | Path not absolute, contains `..` or NUL |
| `INVALID_MODE` | 400 | Bad octal mode string on file write |
| `INVALID_CONTENT` | 400 / 422 | base64 decode failed (400) / unparsable playbook, or one that is not UTF-8 text (422) |
| `PLAYBOOK_TOO_LARGE` | 422 | `GET /api/playbooks/{name}`: the stored file is over the 1 MiB playbook bound. None of it is returned; `GET /api/playbooks` names it with `too_large: true` |
| `FILE_NOT_FOUND` | 404 | File or directory does not exist |
| `FILE_TOO_LARGE` | 400 / 413 | Exceeds `max_file_size` (or the 1 MB playbook cap). Through the relay `GET /api/files` reads at most 4 MiB (`max_file_size` when smaller): a larger file asked with no range is refused before it is read (ask a range), and so is an answer that would not fit the tunnel's 8 MiB body (ask a smaller `limit`) |
| `IS_DIRECTORY` | 400 | Path is a directory where a file was expected |
| `NOT_A_DIRECTORY` | 400 | Upload target is not a directory |
| `PROTECTED_PATH` | 403 | A file read the agent refuses (ADR-011): its own keys and their copies, a process's secrets (`/proc/<pid>/environ` and the like), a kernel stream, a procfs or sysfs file off its list, a debugfs or tracefs file. The message names the rule, never the content. Not worth a retry |
| `NOT_A_REGULAR_FILE` | 400 | A FIFO, a socket or a device, refused before it is opened; on `GET /api/files/raw` and in a transfer, also a listed procfs or sysfs file (the message names its filesystem; `GET /api/files` reads it). The `PROTECTED_PATH` rules come first, so a pseudo file off the list is `PROTECTED_PATH` on every route. Not worth a retry |
| `NOT_FOUND` | 404 | Resource absent — activity result evicted, playbook missing, subsystem not present |
| `PERMISSION_DENIED` | 403 | OS permission error |
| `IO_ERROR` | 500 | Other I/O failure |
| `SESSION_NOT_FOUND` | 404 | No such session |
| `EXEC_FAILED` | 500 | Spawn or wait failure |
| `TIMEOUT` | 504 | Command (`exec_timeout_ms`) or fetch timed out on the device; also the relay proxy timeout (see above) |
| `BATCH_TOO_LARGE` | 400 | More commands than `max_batch_size` |
| `MULTIPART_ERROR` | 400 | Malformed multipart upload |
| `AI_NOT_ALLOWED` | 409 | AI status change on a session the user has not allowed |
| `MODEM_UNAVAILABLE` | 503 | Comms provider not available (not configured, failed to start, or gone) |
| `MODEM_AT_FAILED` | 500 | AT command failure reported by the comms provider |
| `TUNNEL_CONNECTED` | 409 | Band change/scan refused while the tunnel is up (`force: true` overrides) |
| `SCAN_RUNNING` | 409 | A band scan is already in progress |
| `INVALID_URL` | 400 | Unparsable / non-http(s) fetch URL |
| `FETCH_FAILED` | 502 | Fetch connection/protocol failure |
| `CERT_PIN_MISMATCH` | 502 | Presented certificate differs from its recorded pin — **never retry through this**; it is the one signal distinguishing interception from ordinary failure |
| `CERT_UNTRUSTED` | 502 | No CA path, no pin, and TOFU was not requested |
| `SERVER_CONFIG_ERROR` | 500 | The server's own configuration is broken (e.g. no API key set) |
| `TOO_MANY_CONNECTIONS` | 429 | Connection-count limit reached (SSE) |
| `INFRA_UNAVAILABLE` | 404 | Infra monitoring subsystem not available on this device |
| `NET_UNAVAILABLE` | 503 | The device cannot read its network from the kernel (`GET /api/net` in relay mode, or no route socket). A discovery scan then sends nothing, and `GET /api/infra/discover/subnets` keeps `reason: "ip_command_failed"` beside it |

## Comms provider codes

Provider (plugin) failures pass through a status map in the LTE/GPS routes
(`server/src/routes/lte.rs::provider_error`). Codes the provider can raise
beyond the catalog above:

| Code | Status | Meaning |
|------|--------|---------|
| `COMMS_CAPABILITY_UNSUPPORTED` | 501 | The active comms provider does not implement the requested capability (e.g. band control on a provider without it) |

`SCAN_RUNNING` / `TUNNEL_CONNECTED` map to 409, `MODEM_UNAVAILABLE` to 503,
`INVALID_REQUEST` to 400, and anything unrecognized to 500.

## STP transfer codes

STP (gawdxfer) errors reuse the unified shape and always put
`{transfer_id, recoverable}` in `detail`. Status mapping from
`server/src/routes/stp.rs`:

| Code | Status | Meaning |
|------|--------|---------|
| `TRANSFER_NOT_FOUND` | 404 | No such transfer (expired, completed, or never existed) |
| `HASH_MISMATCH` | 400 | Whole-file hash check failed at finalize |
| `CHUNK_INTEGRITY` | 400 | A chunk's hash did not match its header |
| `FILE_CHANGED` | 400 | Source file changed under an active transfer: a chunk is read only from the file the init hashed (the same device and inode, still a regular file, its mtime unchanged). The transfer then fails, gives back its place, and answers every later chunk with this code |
| `DISK_FULL` | 507 | Not enough space to stage the transfer |
| `MAX_TRANSFERS` | 429 | `max_concurrent_transfers` reached; a download takes its place before it hashes |
| `PROTECTED_PATH` | 403 | The read gate refused the download's file (see the device catalog) |
| `NOT_A_REGULAR_FILE` | 400 | The download's file is a FIFO, a socket, a device, or a listed procfs or sysfs file (one off the list is `PROTECTED_PATH`) |

`FILE_NOT_FOUND` (404), `PERMISSION_DENIED` (403), `INVALID_PATH` /
`INVALID_REQUEST` (400) also appear with the same meanings as the device
catalog, and `FILE_TOO_LARGE` (400) for a file over
`transfer_max_file_size`. The tunnel's own transfer frames
(`gx.download.init`) answer `PROTECTED_PATH` with 403 and
`NOT_A_REGULAR_FILE` with 400 too.

## Net tool refusals

`sctl net snmp|http|tcp` refuse a send they would not make on the unit's
LAN side (ADR-012) with exit 3 and one JSON object whose `refused` names
why; the Infra SNMP and Peplink checks put the same code in their detail,
as `SNMP NOT SENT (<code>): ...` or `API NOT SENT (<code>): ...`. They
reach the HTTP surface in an exec's output and in
`GET /api/infra/results`. The request never left, so none is worth a
retry as it stands. The guide's "Network tools" says what each means.

| `refused` | Meaning |
|-----------|---------|
| `not_on_lan` | The kernel would send it through a gateway, by an interface off the LAN side, or nowhere |
| `no_address` | The interface it would leave by holds no IPv4 address |
| `not_unicast` | A broadcast or multicast destination |
| `ipv6` | An IPv6 destination |
| `own_address` | A destination the unit holds on one of its interfaces, up or down, refused before any socket opens: net tools send to devices on its LAN. With `--expect-mac`, the loopback too |
| `mac_mismatch` | Another MAC answered `--expect-mac`'s ARP request, two did, or the kernel's entry names another |
| `no_answer` | Nothing answered `--expect-mac`'s ARP request |
| `config_unreadable` | The agent's config could not be read or parsed |
| `guard_failed` | The tool could not read the routes, ask the wire or bind |

## Restart refusals

`POST /api/restart` (ADR-014) refuses before anything stops, in the unified
shape. `docs/http-api.md` says when each applies.

| `code` | HTTP | Meaning |
|--------|------|---------|
| `RESTART_BAD_REQUEST` | 400 | The body is not a restart request, `request_id` is over 128 bytes, or `unless` holds an unknown word |
| `RESTART_IN_FLIGHT` | 409 | A restart is accepted or its helper is still watching; `detail.restart` is its record. `POST /api/upgrade` answers it too while a restart is in flight |
| `UPGRADE_IN_FLIGHT` | 409 | An upgrade is in progress; `detail.state` says which |
| `RESTART_REFUSED` | 422 | The restart would strand the agent or cut something; `detail.reason` says why |

`detail.reason` of `RESTART_REFUSED`:

| `reason` | Meaning |
|----------|---------|
| `no_install_json` | No `install.json`: the agent does not know how it is started |
| `disabled` | The kill switch (`/etc/sctl/disabled`) exists |
| `loader_unknown` | A `ramboot` unit runs its agent under a loader `install.json` does not name |
| `start_unreadable` | A file the init needs to start is missing or unreadable |
| `cache_invalid` | `ramboot`: the start would download a payload |
| `no_space` | Too little free `/tmp` for the init to start the agent again |
| `relay` | The agent is a relay |
| `recovery_running` | An Infra recovery run is in progress |
| `sessions_open`, `jobs_running`, `transfers_running` | The request's `unless` named that kind, and it is running; `detail.ends` says what |

None is worth a retry as it stands but `recovery_running` and the two
in-flight codes, which pass by themselves.

An accepted restart ends with an `outcome` in its record (`GET
/api/upgrade`'s `restart`), not with an error:

| `outcome` | Meaning |
|-----------|---------|
| `ok` | A new process answered health twice. Written only by the helper |
| `not_restarted` | The process that accepted the request still answers: the restart command did not stop it, or its helper was lost |
| `not_healthy` | No new process answered twice within the window, or the helper reached its hard limit |
| `not_started` | The helper never ran |
| `interrupted` | The agent started again while the restart was in flight and its helper left no outcome: by what it started is not known |

## Relay-only codes

Produced by the tunnel relay (`server/src/tunnel/relay.rs`) — you only see
these on `/d/{serial}/api/*` routes, never from a directly-addressed device:

| Code | Status | Meaning |
|------|--------|---------|
| `DEVICE_NOT_FOUND` | 404 | Serial is not currently registered on this relay |
| `DEVICE_DISCONNECTED` | 502 | Tunnel dropped with the request in flight — `retryable: true` |
| `DEVICE_RECONNECTING` | 502 | Device re-registered mid-request; stale connection drained — `retryable: true` |
| `DEVICE_SEND_FAILED` | 502 | Enqueueing the frame to the device's tunnel writer failed |
| `DEVICE_QUEUE_STALLED` | 502 | The device's send queue did not accept the frame in time |
| `OVERLOADED` | 503 | Too many pending requests for this device (cap 256) |
| `TIMEOUT` | 504 | Device did not answer within the proxy timeout — `retryable: true` |
| `UNEXPECTED_BINARY` | 500 | Device answered a JSON request with a binary frame |
| `DEVICE_RESPONSE_INVALID` | 502 | Device's response frame was malformed (bad `body_b64`, unexpected shape) |
| `DEVICE_DISPATCH_ERROR` | device's status | The device's dispatch layer refused the `http.request` frame (bad path, body too large, streaming endpoint); the relay forwards the device's own status |
| `ROUTE_NOT_PROXIED` | 404 | `/api/ws` and `/api/events` are streaming endpoints and cannot ride the generic request/response passthrough — use `GET /d/{serial}/api/ws` |
| `DEVICE_PAYLOAD_OUTDATED` | 502 | The device's payload predates the generic passthrough (it registered with the legacy `?token=` auth, so it cannot answer `http.request`). Not retryable — the device answers again after it activates a 0.6.0+ payload |
| `PAYLOAD_TOO_LARGE` | 413 | Request body exceeds the 8 MiB tunnel cap |
| `AUTH_MISSING_TOKEN` | 401 | `Authorization` header missing on a proxy route |
| `AUTH_INVALID_TOKEN` | 403 | Key does not match the target device's registered key |
| `INVALID_REQUEST` | 400 | Malformed proxy request (e.g. missing `X-Gx-Chunk-Hash` on a chunk upload) |
| `INTERNAL` | 500 | Relay-side response assembly failed |
