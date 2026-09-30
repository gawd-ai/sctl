# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This root file is the only changelog; per-component history is recorded here
under per-component headings. `server/CHANGELOG.md` is frozen.

## [0.6.6] - 2026-09-30

### sctl (server)

- **`relay_route = "prefer"`**: a third mode names the uplinks to prefer in order (`relay_route_prefer = ["eth0", "usb0"]`), whatever their route metrics. The first listed interface that holds a default route and is not suspect gets the relay route; the probe and suspect machinery are unchanged, so the wire still falls back to LTE when it cannot reach the relay, and returns. For units whose LTE holds the lower metric (the WE826 behind quectel-CM, the BPI behind mwan3), where `follow_default` would keep the relay on the SIM. `/api/health` `tunnel.relay_route.prefer` lists it; the netns test has a prefer leg.
- **Bridge membership is reported**: netwatch reads `IFLA_MASTER`, `net.state` interfaces carry `master` (the bridge a port belongs to), and `/api/info` interfaces carry `master` and `bridge`. A fleet server that reads them no longer runs `brctl show` on every device.
- **`infra.state` is pushed over the tunnel**: the device sends its Infra results (status, latency, since, counters, a short detail, name, http_status per target; no data blobs) after the register ack and whenever a target's status changes or a config is applied or removed, at most one per second, capped at 16 KiB (details go first, then names, then the tail of the id-sorted list, `truncated: true`). Negotiated with `features: ["net.state", "infra.state"]`. A fleet server that reads it no longer pulls `GET /api/infra/results` from every device.

### sctl (relay)

- Keeps the latest `infra.state` per connection, forwards it to the device's WS clients and publishes it on `/api/tunnel/events` (replayed after `device.connected` and `net.state`; dedup key `(relay_epoch, connection_id, state.ts, state.config_version)`). `RELAY_FEATURES` advertises it.

### Packaging

- **WE826**: `install.sh` no longer pins the tunnel to `usb0` by default; it writes `relay_route = "prefer"` with `relay_route_prefer = ["eth0", "usb0"]` (env `RELAY_ROUTE_PREFER`). `TUNNEL_BIND_ADDRESS` still pins a unit that must be pinned and turns `relay_route` off. `netage-wanpref` keeps the default route for passenger traffic; the pin is no longer load-bearing. `devices/we826-qwd/tests/install-test.sh` covers the block written.

### Fixed

- The `infra` module doc said an unconfigured device answers `{"status":"unconfigured"}`; it answers empty results with `config_version: 0`, and the doc says so.

## [0.6.5] - 2026-09-26

### sctl (server)

- **netwatch: the agent reacts to the kernel instead of polling**: one netlink socket listens for link, IPv4 address and IPv4 route events. After one quiet second the agent dumps its links, addresses and main-table routes and publishes a new network state only when it differs; with no events it does no work, and on lost events (`ENOBUFS`) it dumps again. Messages are read and written through byte slices in the host's byte order, never pointer casts, so big-endian MIPS reads them as x86_64 does, and nothing newer than Linux 2.6 is needed (the WE826 runs 3.3.8).
- **The tunnel re-homes in-process**: after connecting, the client records its local and remote addresses and the interface holding the local one (`tunnel.path` in `/api/health` and `/api/info`). When a network change makes the kernel reach the relay from another address, the connection ends with the new `rehome` reason and redials at once, with no agent restart: at most one re-home per 5 s, counted neither as a flap nor toward the backoff. While the tunnel waits to redial, a new route to the relay (a cable plugged in, LTE attaching) ends the wait. A tunnel pinned with `bind_address` does not re-home.
- **`[tunnel] relay_route`** (`"off"` by default, or `"follow_default"`): the agent owns one host route, `<relay>/32 via <gw> dev <uplink> metric 0 proto 83` (and `onlink` when the default route it follows is), on the lowest-metric default route whose uplink answers the relay (the tunnel's own TLS handshake over `wss://`, with its server name, CA file and pin, bound to that uplink and closed straight after, so an uplink that breaks TLS does not answer), chosen again on every network change and after every registration. Two tunnel failures in a row on the held uplink ask it and every other uplink in the same round, and the route moves only when the held uplink does not answer while another does, so a relay outage (nothing answers) moves nothing and marks nothing suspect; the failing one becomes suspect and gets the route back after two good probes, asked at once when its link, address or route changes, no longer suspect once the tunnel registers over it, and otherwise asked on its own schedule, after 2, 5 and 10 minutes, then every 15. An uplink that fails again within 10 minutes of getting the route back waits twice as long each time (never above an hour) until it keeps the route for an hour. Nothing runs on a timer in steady state. A route the agent did not install is never replaced or deleted: the agent stands aside and says so. `/api/health` and `/api/info` report `tunnel.relay_route` (`mode`, `dev`, `via`, `suspect`, and `rp_filter_strict`, the uplinks whose strict reverse-path filtering would drop a probe's answer), and each move is a `relay_route` tunnel event such as `eth1 -> wwan0 (eth1: no answer from the relay)`. Config validation refuses `relay_route` together with `bind_address`. When the relay's name does not resolve (the resolver is usually reached through the same dead uplink), the tunnel dials the relay's known address instead, and a restarted agent keeps counting failures against the route its earlier run left.
- **`net.state`: the agent pushes its network**: the device offers `"features":["net.state"]` in `tunnel.register`. When the relay's ack lists it too, the device sends a `net.state` message after the ack and after every change whose content differs, at most one a second: interfaces, default routes, the kernel's route to the relay, the tunnel's two ends and the route to each WireGuard peer, stamped with `boot` and `seq` and capped at 16 KiB. `GET /api/net` returns the same message built from a fresh dump (`seq` 0), or `503 NET_UNAVAILABLE`.
- **`GET /api/tunnel/events` on the relay**: a WebSocket for up to 8 subscribers, authenticated with `Authorization: Bearer <tunnel_key>` only (a missing or wrong key gets 403, a ninth subscriber 429). It sends `hello`, then a replay of every connected device (`device.connected`, then its latest `net.state`), `replay.done`, then live `device.connected`, `device.disconnected` and `net.state` frames; a subscriber that falls behind gets `resync` and a fresh replay. The relay keeps each connection's latest `net.state`, forwards it to the device's `/d/{serial}/api/ws` clients, and lists each device's `features` in `/api/tunnel/devices`.

### devices

- **XE300: the agent owns the relay route**: `install.sh` writes `[tunnel] relay_route = "follow_default"` (`RELAY_ROUTE`; never beside a `bind_address` pin) and removes the `96-wg-repin` and `97-sctl-rehome` hotplugs, which pinned the relay /32 and restarted sctl on wan ifup, together with the pin they left. They are kept in a backup directory. It also makes `rp_filter` loose (2) for `all`, `default` and every interface, at once and in `/etc/sysctl.conf` for every boot, so the answer to the agent's probe of an uplink the route does not use is not dropped. The shared helper is `devices/xe300/relay-route.sh`.
- **`rundev.sh device upgrade-remote` works on the XE300**: it asks the device how sctl is installed, ships the OpenWrt-SDK payloads from `devices/xe300/build.sh` over STP, refuses a tunnel pinned with `bind_address` and says where `rp_filter` is strict. `devices/xe300/upgrade.sh` then runs on the device: it keeps the previous payloads, `sctl.toml` and hotplugs in `/usr/local/lib/sctl/rollback`, sets `relay_route` in `[tunnel]` with every other line left as it is, removes the hotplugs, makes `rp_filter` loose (which a rollback leaves), restarts the agent once, and puts everything back unless the agent returns with the new version and a connected tunnel within 180 s. `RELAY_ROUTE=off` upgrades the payloads only. The chunked STP upload is now one function that the generic path shares.

## [0.6.4] - 2026-09-24

### web (sctlin)

- **Playbook components are themeable**: `PlaybookViewer` and `PlaybookExecutor` take every colour, font and size from `--sctl-*` custom properties whose defaults are the existing dark look, so a consuming app can restyle them to its own palette (light or dark) without forking. The run output has its own pair (`--sctl-output-bg`, `--sctl-output-text`) so it can stay terminal-dark while the rest follows the page.
- **Host-driven runs**: the viewer takes `header={false}`; the executor takes `actions={false}` and `description={false}`, exposes `run()` and `cancel()` on the instance and reports `PlaybookRunState` through `onstatechange`, so a host can put Run and Stop in its own toolbar. Parameter labels are now `<label for>` their fields.

### sctl (server)

- The closed-port `tcp_port` test holds a bound, non-listening socket instead of assuming a port is free.

## [0.6.3] — 2026-09-15

### sctl (server)

- **`http`/`https` infra checks no longer transfer the page** — the check sends HEAD and reads the status and headers; only a server that refuses HEAD (405, 501) gets a GET, and that GET asks for one byte (`Range: bytes=0-0`, a 206 counts as the expected 200). Latency is now time to first byte. Before, `curl -o /dev/null` still pulled the whole body: a 60 s check of a gateway's 82 KB page cost 118 MB/day on the site's WAN (RUT241, Canphone).
- **`tcp_port` checks connect natively** — no more `nc -z -w`, which the busybox nc on RUTOS does not accept (every tcp_port target on a RUT241 was DOWN). A `TcpStream::connect` with a deadline; its duration is the latency.
- **Peplink profile** — when firmware 8.6.0 ships `rat[]` entries without a `name`, the cellular WAN's `rat` list falls back to `dataTechnology`, then `mobileType`, instead of reading blank.

### devices

- **Install templates** — the WE826 template wrote `bind_address = ""` when `TUNNEL_BIND_ADDRESS` was empty, which the tunnel treats as an unavailable interface and retries forever; the line is now conditional, as on the XE300. Both templates and the WE826 README say why a pin on the cellular interface is a metering decision: it puts the whole management plane on the SIM.

## [0.6.2] — 2026-09-07

### sctl (server)

- **`state_dir`** — a directory for the few files that must survive a reboot even where `data_dir` is tmpfs: `infra-monitor.json`, `infra-secrets.json`, `tls_pins.json`. Defaults to `data_dir`; the WE826 and RUT241 profiles set `/etc/sctl/state`. Bus 01 lost all eight infra targets and its router credential at every ignition cycle.
- **Infra results tell the truth at boot** — a loaded config is reflected in `GET /api/infra/results` at once (its version, one `unknown` entry per target) instead of `config_version: 0` and no targets until the first check lands; `0` now means the device holds nothing. A config push drops the targets that left.
- **Peplink profile** — cellular WANs carry `network` (RAT in use), `rat`, `bands`, `signal_level`, `carrier_aggregation`, `sim_active` and `roaming`; the summary line names the network and breaks the latency down (`login`, `wan`, `reads`).

## [0.6.1] — 2026-09-07

### sctl (server)

- **`http_api` infra check kind with vendor profiles** — an Infra target can now log into a device's JSON management API over sctl's own TLS stack (pinned certificate, in-process `fetch` engine with typed errors), keep the session, and reduce the answers on the device to a structured `data` snapshot. First profile: Peplink / Pepwave (MAX BR1 family); the reduction keeps passenger identifiers on the device.
- **Infra credentials store** — `POST/GET /api/infra/credentials`, `DELETE /api/infra/credentials/{id}`; credentials live in an owner-only `infra-secrets.json`, never in the monitoring config or a reply.
- **Infra results carry `data` and `http_status`**; a failed check keeps the last good snapshot; `GET /api/infra/history/{target_id}` returns a bounded ring of readings for a collector that was cut off.
- **Concurrent infra checks** under a small semaphore with a per-check deadline, so a multi-request profile cannot stall ping targets.

## [Unreleased] — 0.6.0 cycle

### sctl (server)

#### Resilience payload

- **Supervision on procd-less units** — the RAM-boot init's legacy branch now runs a respawn loop instead of launching the daemon once and walking away; a crash no longer strands a unit until its next ignition cycle.
- **`panic = unwind`** — most panics become recoverable in-process instead of aborting the daemon (size delta flagged against the revisit clause).
- **Jittered tunnel reconnects; a rejected tunnel key is never fatal** — the client retries on a slow cadence instead of stopping (a tunnel-only device that stops retrying is unreachable until someone drives to it).
- **Panic-proof request paths** — no filename can panic a file download, no request can panic the sctlin proxy.
- **AT serial fd leak fixed** — the AT layer owns its serial fd; a failed open no longer leaks one per attempt.
- **PTY slaves leave canonical mode** — session PTYs no longer run under the cooked-mode line discipline.
- **WE826 payload hardening** — AT-port detection baked in (no hardcoded `ttyUSB*`, survives re-install); `install.sh` no longer silently un-pins the tunnel on re-install; new `netage-wanpref` wired-WAN preference agent.

#### Relay history

- **Connection history survives relay restarts** — persisted under `data_dir`; rings are per-device; sessions close by identity, not by slot.
- **Transport-path recording** — the relay records which path a device's tunnel arrived over.
- History extracted into its own `tunnel/history.rs` unit.

#### Transport contract

- **Generic `http.request` passthrough** — the relay forwards any `/d/{serial}/api/*` request as a frame the device dispatches into its own router; the hand-written per-endpoint proxy wrappers are deleted. New device endpoints (including remote safe-mode clearing on CGNAT units) are reachable the moment the device ships them, with no relay change.
- **One error dialect** — every error body carries `code` + `message` (legacy `error` duplicate kept until 0.7.0); relay transport-loss errors carry `retryable: true` (`DEVICE_DISCONNECTED` / `DEVICE_RECONNECTING` / `TIMEOUT`).
- **Tunnel key moves to the Authorization header** — logs never see it; the query-param fallback remains for pre-0.6.0 payloads and goes away in 0.7.0. A rotated key now rejects at the WS upgrade, not in-band.
- **Relay hardening** — device-health oracle closed, device-supplied headers sanitized, JSON on every miss.
- **`session.exited` joins the typed `WsServerMsg` enum**; `session.gap` documentation corrected.

#### Features

- **`POST /api/fetch`** — device-side HTTP(S) fetch, making sctl's TLS stack (extra CAs, cert pinning) usable by the whole device.
- **`/api/info` storage and GPS** — all storage volumes reported via `disks[]` (one mount point, one row); GPS course projected from data the driver already parsed.

#### Workspace & CI

- **One cargo workspace, one lockfile** — CI compiles every crate, including the comms ABI and the Quectel driver (1.6k lines of unsafe FFI that previously never met CI).
- **One lint policy** for all workspace members; toolchain pinned (1.97.1) with a floating-stable canary job that warns instead of breaking main.

#### Docs

- Hand-written HTTP API reference (`docs/http-api.md`) with a CI drift gate against the registered routes.
- Error-model reference (`docs/errors.md`): unified shape, `retryable` semantics, complete code catalog.
- Config reference (`docs/config.md`) + fully-keyed `sctl.toml.example`, with their own CI drift gate.
- Safe-mode operator runbook (`docs/safe-mode.md`) and build/deploy/release reference (`docs/releasing.md`).
- `docs/README.md` reading-path index; `guide.md` stops promising tunnel-internal `gps.fix` / `lte.signal` frames to WS clients.

## [0.5.0] - 2026-05-26

### sctl (server) v0.5.0

#### Performance

- **Async filesystem in polling loops** — replaced `std::fs` with `tokio::fs` across LTE poller, watchdog history, and modem-state log (28 sites). Introduced shared `util::append_rotating` helper for both `watchdog_history.jsonl` and `modem-state.log` append-and-rotate pattern. Runs on the blocking pool via `spawn_blocking` to avoid worker-thread stalls.
- **Smaller server artifact** — removed vendored OpenSSL and the duplicate websocket stack, moved tunnel TLS onto direct rustls/ring with optional CA/pin checks, trimmed unused dependency defaults, replaced the two-command Clap CLI with a tiny parser, simplified release logging, and tuned the release profile for size. The local x86_64 release binary dropped from ~11.34 MB to ~4.93 MB while retaining WSS support.
- **Relay broadcast fan-out** — payloads wrapped in `Arc<serde_json::Value>` so the per-client dispatch loop clones the Arc instead of the full JSON tree. Eliminates the relay's hottest allocation under sustained tunnel traffic.
- **Priority queue capacity** — relay control-channel `mpsc::channel` bumped from 8 to 32 with a >75%-full warn log so future backpressure surfaces instead of silently dropping.
- **`band_scan` extracted from `lte.rs`** — band-scan orchestration and safe-bands persistence moved to `lte/band_scan.rs`. `lte.rs` is back to a reasonable size; public API preserved.
- **Lint hygiene** — dropped unused `clippy::pedantic` allow entries, re-enabled `must_use_candidate` with annotations on public state-returning functions.

#### Added

- **External comms provider plugins** — GPS/LTE hardware support now runs through C-ABI shared libraries. The main `sctl` binary no longer carries Quectel modem code; `libsctl_comms_quectel.so` is deployed only to targets that need the current LTE/GNSS provider.
- **Rebuilt autonomous LTE watchdog (server-side).** The recovery state machine was redesigned for the plugin architecture and now runs in the server, driving the plugin's AT/recovery primitives. It acts only when the tunnel is down, the client is not mid-reconnect, and grace has elapsed; diagnoses the fault from kernel truth (interface IPv4 + interface-bound ping); and never escalates on an RF outage (storm/dead zone), where it waits instead of cycling. The USB power-cycle stays opt-in (`max_escalation_level = 4`) behind sustained-evidence, sysfs-present, exponential-backoff, and dormant-after-three-failures gates. The airplane-mode cycle always restores `CFUN=1`, and startup recovery (`CFUN=1` + USB re-authorize) heals any action a crash interrupted. Dropped accumulated cruft: `at_test_mode`, `active_poll_interval_secs`, `inter_command_delay_ms`, `apn`, `unknown_action`, `usb_cycle_evidence.require_sysfs_absent`, and the legacy L0–L3 level vocabulary. The watchdog no longer manipulates band configuration.
- **Opt-in OpenWrt persistent logs** — embedded devices keep logd RAM-only by default. Operators can enable bounded local post-crash logs with `openwrt_persistent_logs = true` and `openwrt_persistent_log_size_kb`.
- **Unified `ApiError` + `codes` catalog** — every route returns `Result<Json<T>, (StatusCode, Json<ApiError>)>` with stable SCREAMING_SNAKE error codes from `error::codes`. Replaces the prior mix of `{error, code}` and `{code, message}` shapes across exec, files, lte, auth, sessions, ws. ~115 call sites migrated.
- **Typed `WsServerMsg` enum** — serde internally-tagged enum replaces 38 hand-built `json!()` sites. Wire format unchanged; compile-time exhaustiveness on the server side.
- **Generated TS bindings** — `ts-rs` annotations on `WsServerMsg`, `ApiError`, transfer types, activity types. Generated `.ts` files replace hand-maintained type duplicates in the web client; bindings regenerate on `cargo test export_bindings`.
- **Transfer event observability** — `gawdxfer` transfers now log `transfer_start` and `transfer_complete` to the activity journal with structured detail (transfer_id, direction, filename, file_size, total_chunks). Web client subscribes to `gx.progress` and `gx.complete` via new `onTransferProgress`/`onTransferComplete` hooks on `WsClient`.

#### Fixed

- **`util::append_rotating` CI test race** — switched from `tokio::fs` to `spawn_blocking` + `std::fs` with explicit `flush()`. The tokio async-file `Drop` schedules close on the blocking pool; readers racing the close occasionally observed an empty file on fast filesystems (caught by CI).
- **LTE watchdog no longer condemns a healthy bearer on AT `NOCONN`** — on the EC25 in QMI raw-IP mode, `AT+QENG="servingcell"` reports `NOCONN` even while a live bearer carries traffic. `diagnose()` now decides the data path from kernel truth (interface IPv4 + interface-bound reachability ping) instead of the serving-cell field: no IPv4 → recoverable `RegisteredNoData`; IPv4 + relay reachable → `RelayProblem` (modem untouched); IPv4 + unreachable → `InternetUnreachable` (diagnosed, not actionable). Prevents a spurious interface-restart/USB-cycle escalation — important now that the tunnel can ride a non-LTE WAN, where a tunnel drop no longer implies an LTE fault.
- **Truthful `connection_state`** — the LTE poller reports `CONNECT` when the interface has an IPv4 (bearer up), overriding the unreliable AT idle states (`NOCONN`/`LIMSRV`/`SEARCH`).
- **`band`/`operator` no longer read null in deep idle** — `AT+QNWINFO`/`AT+COPS?` go dark when the EC25 idles, but the QENG serving cell (already polled every cycle) still reports the `freq_band` and PLMN while camped. `band` now falls back to `B{freq_band}` and `operator` to a PLMN→name lookup (raw PLMN when unknown). No extra AT polling — purely derived from data already fetched.

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
