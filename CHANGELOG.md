# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-05-26

### sctl (server) v0.5.0

#### Performance

- **Async filesystem in polling loops** — replaced `std::fs` with `tokio::fs` across LTE poller, watchdog history, and modem-state log (28 sites). Introduced shared `util::append_rotating` helper for both `watchdog_history.jsonl` and `modem-state.log` append-and-rotate pattern. Runs on the blocking pool via `spawn_blocking` to avoid worker-thread stalls.
- **Relay broadcast fan-out** — payloads wrapped in `Arc<serde_json::Value>` so the per-client dispatch loop clones the Arc instead of the full JSON tree. Eliminates the relay's hottest allocation under sustained tunnel traffic.
- **Priority queue capacity** — relay control-channel `mpsc::channel` bumped from 8 to 32 with a >75%-full warn log so future backpressure surfaces instead of silently dropping.
- **`band_scan` extracted from `lte.rs`** — band-scan orchestration and safe-bands persistence moved to `lte/band_scan.rs`. `lte.rs` is back to a reasonable size; public API preserved.
- **Lint hygiene** — dropped unused `clippy::pedantic` allow entries, re-enabled `must_use_candidate` with annotations on public state-returning functions.

#### Added

- **External comms provider helpers** — GPS/LTE hardware support now runs through a helper-process protocol. The main `sctl` binary no longer carries Quectel modem code; `sctl-comms-quectel` is deployed only to targets that need the current LTE/GNSS provider.
- **Unified `ApiError` + `codes` catalog** — every route returns `Result<Json<T>, (StatusCode, Json<ApiError>)>` with stable SCREAMING_SNAKE error codes from `error::codes`. Replaces the prior mix of `{error, code}` and `{code, message}` shapes across exec, files, lte, auth, sessions, ws. ~115 call sites migrated.
- **Typed `WsServerMsg` enum** — serde internally-tagged enum replaces 38 hand-built `json!()` sites. Wire format unchanged; compile-time exhaustiveness on the server side.
- **Generated TS bindings** — `ts-rs` annotations on `WsServerMsg`, `ApiError`, transfer types, activity types. Generated `.ts` files replace hand-maintained type duplicates in the web client; bindings regenerate on `cargo test export_bindings`.
- **Transfer event observability** — `gawdxfer` transfers now log `transfer_start` and `transfer_complete` to the activity journal with structured detail (transfer_id, direction, filename, file_size, total_chunks). Web client subscribes to `gx.progress` and `gx.complete` via new `onTransferProgress`/`onTransferComplete` hooks on `WsClient`.

#### Fixed

- **`util::append_rotating` CI test race** — switched from `tokio::fs` to `spawn_blocking` + `std::fs` with explicit `flush()`. The tokio async-file `Drop` schedules close on the blocking pool; readers racing the close occasionally observed an empty file on fast filesystems (caught by CI).
- **LTE watchdog no longer condemns a healthy bearer on AT `NOCONN`** — on the EC25 in QMI raw-IP mode, `AT+QENG="servingcell"` reports `NOCONN` even while a live bearer carries traffic. `diagnose()` now decides the data path from kernel truth (interface IPv4 + interface-bound reachability ping) instead of the serving-cell field: no IPv4 → recoverable `RegisteredNoData`; IPv4 + relay reachable → `RelayProblem` (modem untouched); IPv4 + unreachable → `InternetUnreachable` (diagnosed, not actionable). Prevents a spurious interface-restart/USB-cycle escalation — important now that the tunnel can ride a non-LTE WAN, where a tunnel drop no longer implies an LTE fault.
- **Truthful `connection_state`** — the LTE poller reports `CONNECT` when the interface has an IPv4 (bearer up), overriding the unreliable AT idle states (`NOCONN`/`LIMSRV`/`SEARCH`). `band`/`operator` may still read null in this deep-idle state — an AT serving-cell reporting limitation, not a connectivity problem.

### mcp-sctl v0.5.0

#### Changed

- Unified package version with the sctl 0.5.0 release; consumes the unified `ApiError` shape via shared types.

### sctlin (web) v0.5.0

#### Added

- **Hacker theme** — `web/src/lib/styles/theme.css` overrides gawdux CSS variables with sctlin's terminal palette (deep neutrals, phosphor accents). Imported after `gawdux/styles/tokens.css` in `app.css`.
- **gawdux 0.2.0 adoption** — `DarkModeToggle`, `PageFeedback`, `ListPageScaffold` + `TableContainer` + `PageActionBar` from gawdux primitives. Replaces local `ToastContainer` and refactors `ServerDashboard` onto the shared scaffold.
- **Generated TS types** — replaces hand-maintained `WsServerMsg`/`ApiError`/`ActivityType` shapes with bindings emitted from the Rust server.
- **`onTransferProgress` / `onTransferComplete` hooks** on `WsClient` for `gx.*` event subscription.

#### Changed

- **gawdux pin** — `package.json` now pins gawdux to commit `56ef7fa` (v0.2.0) instead of a floating GitHub URL; installs are deterministic.
- **Utility dedupe** — `moduleToMenuItem`, `buildGroupItems`, and related helpers now imported from `gawdux/utils` instead of duplicated in `+page.svelte`.

### gawdux v0.2.0 (consumed by sctlin)

#### Added

- **CSS variable theming layer** — `tokens.css` consumes `--gawdux-*` custom properties with SIMS-palette defaults (existing consumers unchanged). New `theme.css` declares the defaults and is the override surface for downstream consumers.
- **`className` forwarding** on every primitive — consumers can override styles without specificity wars.

## [0.4.0] - 2026-05-21

### sctl (server) v0.4.0

#### Added

- **Playbook REST API** — dedicated `/api/playbooks` CRUD endpoints with server-side YAML frontmatter validation.
  - `GET /api/playbooks` — list playbooks with name, description, params.
  - `GET /api/playbooks/:name` — get full playbook detail (metadata, params, script, raw content).
  - `PUT /api/playbooks/:name` — create/update with server-side validation.
  - `DELETE /api/playbooks/:name` — delete playbook.
- **Playbook activity types** — `PlaybookList`, `PlaybookRead`, `PlaybookWrite`, `PlaybookDelete` in activity journal.
- **Tunnel proxy** — playbook endpoints proxied through relay at `/d/{serial}/api/playbooks*`.
- **Tunnel client** — handles `tunnel.playbooks.*` messages for proxied playbook operations.
- **Config** — added `playbooks_dir` setting (default: `/etc/sctl/playbooks`).
- **Reverse tunnel** — built-in relay for CGNAT devices. Any sctl instance can act as a relay; devices connect outbound via WebSocket and clients reach them through standard API URLs (`/d/{serial}/api/*`).
- **AI status tracking** — `session.allow_ai`, `session.ai_status`, and broadcast events for real-time AI/human collaboration UI.
- **Session rename** — `session.rename` message with broadcast to all connected clients.
- **TLS via rustls** — switched from native-tls to rustls for TLS support.
- **Tunnel reliability** — drain pending requests on disconnect, heartbeat sweep, backpressure, structured logging.
- **GPS location tracking** — `[gps]` config section, `GET /api/gps` endpoint, GPS summary in `/api/health`, GPS data in `/api/info`, `gps.fix` WebSocket broadcast.
- **LTE signal monitoring** — `[lte]` config section, signal quality and modem info in `/api/info`, `lte.signal` WebSocket broadcast.
- **Shared AT command infrastructure** — `modem.rs` with per-device serial port mutex for GPS and LTE to share the modem safely.
- **Activity REST endpoints** — `GET /api/activity` with filtering (since_id, limit, activity_type, source, session_id), `GET /api/activity/{id}/result` for cached exec results.
- **File delete endpoint** — `DELETE /api/files` with path validation and permission checks.
- **REST session management** — `GET /api/sessions`, `DELETE /api/sessions/{id}`, `PATCH /api/sessions/{id}` (rename, AI toggle), `POST /api/sessions/{id}/signal`, `GET /api/shells`.
- **Enhanced `/api/health`** — includes `sessions` count, conditional `tunnel` object with full metrics (messages, RTT, events), conditional `gps` summary.
- **Enhanced `/api/info`** — includes conditional `tunnel` status, `gps` fix data, `lte` signal + modem info.
- **Tunnel `bind_address`** — bind outbound WS to a specific interface or IP for LTE failover.
- **Tunnel resilience** — flap detection (3 connections <30s triggers 60s backoff), channel-based WS sink (replaces mutex), TunnelStats with atomics, pong RTT tracking with median/p95, writer exit detection via oneshot, subscriber task reaping, panic boundaries on spawned handlers.
- **Activity logging for tunnel exec** — `_source` forwarding from proxied requests.
- **New tunnel proxy endpoints** — file delete, activity, exec results, sessions, shells, playbooks, GPS at `/d/{serial}/api/*`.
- **Library crate refactoring** — `lib.rs`, `state.rs` for shared types.

### mcp-sctl v0.2.0

#### Added

- **Playbook REST methods** — `list_playbooks()`, `get_playbook()`, `put_playbook()`, `delete_playbook()` on `SctlClient`.
- **API-first playbook loading** — uses `/api/playbooks` endpoint when available, falls back to file-based approach for older servers.
- **AI status auto-management** — MCP proxy auto-sets `working=true` before session commands (activity=write for exec/send, activity=read for read). Auto-cleared by server after 60s inactivity.
- **Session auto-routing** — sessions automatically routed to the correct device.
- **Config version 2** — `config_version` bumped to 2. Extra metadata fields (`host`, `serial`, `arch`, `sctl_version`, `added_at`) are accepted and ignored by mcp-sctl, used by `rundev.sh device` commands.
- **`device_gps` tool** — GPS location data (fix, history, status) from devices with `[gps]` configured.
- **`device_file_delete` tool** — delete files on a device.
- **`device_activity` tool** — read the activity log with since_id/limit filtering.
- **Chunked file upload** — auto-switches to chunked upload for files >2 MB via gawdxfer STP.

### sctlin (web) v0.2.0

#### Added

- **HistoryViewer** — full-panel activity viewer with type/source filter chips, text search, multi-expand, load-more pagination.
- **PlaybookList** — playbook browser with name, description, param count badge, select/delete/create/refresh actions.
- **PlaybookViewer** — playbook detail view with metadata header, parameter table, script block, execute/edit buttons.
- **PlaybookExecutor** — parameter form with auto-populated defaults, live script preview, execution output display.
- **Widgets** — new `sctlin/widgets` export path with self-contained components:
  - `TerminalWidget` — wraps `TerminalContainer` with simplified config.
  - `DeviceStatusWidget` — device info with polling, loading/error states.
  - `ActivityWidget` — activity feed with REST fetch + real-time WebSocket updates.
  - `PlaybookWidget` — playbook browser + viewer + executor with REST client.
- **REST client** — added `getHealth()`, `listPlaybooks()`, `getPlaybook()`, `putPlaybook()`, `deletePlaybook()`.
- **Types** — `HistoryFilter`, `PlaybookParam`, `PlaybookSummary`, `PlaybookDetail`, `DeviceConnectionConfig`.
- **playbook-parser.ts** — client-side playbook frontmatter parsing, script rendering, name validation.
- **Vitest** — test infrastructure with `@testing-library/svelte`.
- **LTE signal panel** — bars indicator, operator, band, RSRP/SINR metrics in ServerDashboard.
- **GPS status panel** — coordinates, satellites, fix age in ServerDashboard.
- **Network interface filter** — handles wwan0/UNKNOWN operstate correctly.

#### Fixed

- Exported 13 previously missing WS message types from barrel files.
- Moved `flowbite-svelte`, `flowbite-svelte-icons`, `gawdux` to `devDependencies`.
- Removed `@sveltejs/kit` from `peerDependencies` (library is pure Svelte 5).
- Removed `gawdux` dependency from `ServerPanel.svelte` (inlined flyout as positioned div).

### rundev.sh

#### Added

- **Device management** — `device add`, `device rm`, `device ls`, `device deploy`, `device upgrade` subcommands for discovering, deploying, and managing physical devices.
- **Enhanced tunnel mode** — `rundev.sh tunnel` connects all configured physical devices via SSH tunnel, not just a local client. Cleanup on Ctrl+C restores all devices to normal operation.
- **Shared helpers** — `wait_for_health`, `start_web_dev_server`, config helpers (`cfg_get`, `cfg_device_get`, etc.), architecture mapping.
- **Architecture auto-detection** — SSH probe discovers device arch and maps to cross-compile target (`riscv64`, `armv7l`, `aarch64`, `x86_64`).

#### Fixed

- **Tunnel devices API auth** — `do_status` and `do_tunnel` now use `?token=` query param instead of incorrect `Authorization: Bearer` header for the relay's device list endpoint.

## [0.3.0] - 2026-02-06

### sctl (server)

#### Added

- **PTY support** — `session.start` accepts `pty: true` for full terminal emulation with ANSI escape codes, cursor movement, colors, and interactive TUI programs.
- **Session resize** — `session.resize` message to change PTY terminal dimensions (rows/cols).
- **Output journaling** — optional disk-backed persistence for session output, enabling crash recovery of persistent sessions. Configurable max age for automatic cleanup.
- **Session list** — `session.list` message to enumerate active sessions with status.

### mcp-sctl (MCP proxy) — initial release

#### Added

- **Device tools** (HTTP): `device_list`, `device_health`, `device_info`, `device_exec`, `device_exec_batch`, `device_file_read`, `device_file_write`.
- **Session tools** (WebSocket): `session_start`, `session_exec`, `session_send`, `session_read`, `session_signal`, `session_kill`, `session_attach`, `session_resize`, `session_exec_wait`, `session_list`.
- **Multi-device support** — JSON config with named devices and per-device API keys.
- **Local output buffering** — session output cached in-process for zero-latency reads.
- **Auto-reconnect** — WebSocket disconnects trigger exponential backoff reconnect with sequence-based re-attach. No output lost.
- **Playbook discovery** — device-stored markdown playbooks automatically exposed as dynamic MCP tools (`pb_*`).
- **`session_exec_wait`** — execute a command and wait for completion in a single call using marker-based detection.
- **Claude Code integration** — `rundev.sh` for one-command development environment setup.

## [0.2.0] - 2026-02-06

### sctl (server)

#### Added

- **Persistent sessions** — `session.start` gains `persistent: true` flag. Persistent sessions survive WebSocket disconnects; output keeps buffering for later re-attach.
- **Session re-attach** — `session.attach` message. Clients send `session_id` + `since` (last seen seq), server replays missed output from the ring buffer.
- **Process group signals** — `session.signal` message. Sessions spawned with `setpgid(0, 0)`, signals sent to `-pgid` reach the entire process tree (real Ctrl-C).
- **Buffer-backed sessions** — output goes to `OutputBuffer` ring buffer (configurable, default 1000 entries) instead of being coupled to the WebSocket.
- **Sequenced output** — `session.stdout`, `session.stderr`, `session.system` include `seq` and `timestamp_ms` for reliable ordering and catch-up.
- **Config** — `session_buffer_size` and `detach_timeout` settings.

#### Changed

- Session I/O decoupled from WebSocket — output goes to buffer, subscriber task forwards to WS.
- Non-persistent sessions killed on WS disconnect (backward compatible). Persistent sessions detached.
- Sweep task cleans up detached persistent sessions past `detach_timeout`.

## [0.1.0] - 2026-02-05

### sctl (server)

#### Added

- **HTTP API**: health, system info, command execution (single + batch), file read/write with symlink detection.
- **WebSocket API**: interactive shell sessions with start/kill, exec, stdin, streaming stdout/stderr, exit notification, ping/pong, request_id correlation.
- **Authentication**: pre-shared API key with constant-time comparison, Bearer header for HTTP, query param for WebSocket.
- **Configuration**: TOML file with environment variable overrides.
- **Resource limits**: max_sessions, session_timeout, exec_timeout_ms, max_batch_size, max_file_size.
- **Security**: path traversal prevention, kill_on_drop, TOCTOU-safe session creation, pipe deadlock prevention, atomic file writes.
- **Graceful shutdown**: SIGINT/SIGTERM handling, clean session teardown.
- **OpenWrt deployment**: procd init script, ARM cross-compilation via `cross`, Makefile deploy target.
