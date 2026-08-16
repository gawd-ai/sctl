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
| `INVALID_CONTENT` | 400 / 422 | base64 decode failed (400) / unparsable playbook (422) |
| `FILE_NOT_FOUND` | 404 | File or directory does not exist |
| `FILE_TOO_LARGE` | 400 / 413 | Exceeds `max_file_size` (or the 1 MB playbook cap) |
| `IS_DIRECTORY` | 400 | Path is a directory where a file was expected |
| `NOT_A_DIRECTORY` | 400 | Upload target is not a directory |
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
| `FILE_CHANGED` | 400 | Source file changed under an active transfer |
| `DISK_FULL` | 507 | Not enough space to stage the transfer |
| `MAX_TRANSFERS` | 429 | `max_concurrent_transfers` reached |

`FILE_NOT_FOUND` (404), `PERMISSION_DENIED` (403), `FILE_TOO_LARGE` /
`INVALID_PATH` / `INVALID_REQUEST` (400) also appear with the same meanings
as the device catalog.

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
| `PAYLOAD_TOO_LARGE` | 413 | Request body exceeds the 8 MiB tunnel cap |
| `AUTH_MISSING_TOKEN` | 401 | `Authorization` header missing on a proxy route |
| `AUTH_INVALID_TOKEN` | 403 | Key does not match the target device's registered key |
| `INVALID_REQUEST` | 400 | Malformed proxy request (e.g. missing `X-Gx-Chunk-Hash` on a chunk upload) |
| `INTERNAL` | 500 | Relay-side response assembly failed |
