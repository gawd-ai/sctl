# sctl configuration reference

Every TOML key `sctl serve` reads, with its compiled default. Source of
truth: `server/src/config.rs`. Covers sctl 0.6.0.

This file is gated by CI: `scripts/check-config-docs.py` cross-checks every
struct field in `config.rs` against this file and against
`server/sctl.toml.example` — add a config field without documenting it here
and CI fails. **Key contract:** each field appears in a table row starting
with `` `name` `` in the section for its struct.

A fully-commented example lives at
[`server/sctl.toml.example`](../server/sctl.toml.example); a relay-profile
example at [`server/relay.toml.example`](../server/relay.toml.example).

## Contents

- [Precedence and loading](#precedence-and-loading)
- [Validation](#validation)
- [`[server]`](#server)
- [`[auth]`](#auth)
- [`[shell]`](#shell)
- [`[device]`](#device)
- [`[logging]`](#logging)
- [`[supervisor]`](#supervisor)
- [`[tunnel]`](#tunnel)
- [`[comms]`](#comms)
- [`[gps]`](#gps)
- [`[lte]`](#lte)

## Precedence and loading

Configuration is resolved highest-wins:

1. **Environment variables** — `SCTL_API_KEY`, `SCTL_LISTEN`,
   `SCTL_DEVICE_SERIAL`, `SCTL_DATA_DIR`, `SCTL_PLAYBOOKS_DIR`
   (and `RUST_LOG` for the log level).
2. **Config file** — the path given with `--config <path>`, otherwise
   `sctl.toml` in the current working directory if present.
3. **Compiled defaults** — the values listed below.

Every section is optional. `[server]`, `[auth]`, `[shell]`, `[device]`,
`[logging]`, and `[supervisor]` fall back to defaults field-by-field;
`[tunnel]`, `[comms]`, `[gps]`, and `[lte]` are **absent-means-disabled** —
omit the whole section to turn the subsystem off.

## Validation

`Config::validate()` runs at startup; any failure logs every error and
exits. The rules:

- `server.listen` must parse as a socket address.
- `server.default_terminal_rows` / `_cols` in `[1, 500]`.
- `server.max_sessions` at most 10 000.
- `server.max_file_size` and `server.transfer_chunk_size` at least 1024.
- `server.max_concurrent_transfers` at least 1.
- `tunnel.url` must start with `ws://` or `wss://`; the TLS options
  (`tls_ca_file`, `tls_server_cert_sha256`) require a `wss://` URL.
- Relay mode requires `tunnel.tunnel_key` of at least 8 characters.
- `tunnel.relay_route = "follow_default"` cannot be combined with
  `tunnel.bind_address`: one pins the tunnel to an interface, the other moves
  the relay's route between uplinks.
- `[gps]` or `[lte]` require an enabled `[comms]` section (see below).

## `[server]`

HTTP server and resource limits (`ServerConfig`).

| Key | Default | Description |
|-----|---------|-------------|
| `listen` | `"0.0.0.0:1337"` | Socket address to bind. Env: `SCTL_LISTEN`. |
| `max_connections` | `10` | Maximum concurrent TCP connections (tower `ConcurrencyLimitLayer`). |
| `max_sessions` | `20` | Maximum concurrent WebSocket shell sessions. |
| `exec_timeout_ms` | `30000` | Default timeout for `POST /api/exec`, milliseconds. |
| `include_interface_addresses_in_info` | `true` | Whether `/api/info` enumerates interface IP addresses on demand. Set `false` to keep interface names/state/MAC but skip request-path address discovery on fragile network stacks. |
| `max_batch_size` | `20` | Maximum commands per `POST /api/exec/batch`. |
| `max_file_size` | `52428800` (50 MB) | Maximum file size in bytes for `/api/files` read/write. |
| `session_buffer_size` | `1000` | Maximum output entries kept per session ring buffer. |
| `data_dir` | `"/var/lib/sctl"` | Directory for persistent data — journals, safe-mode flag, panic markers, relay connection history. Env: `SCTL_DATA_DIR`. |
| `state_dir` | `""` (= `data_dir`) | Directory for the few small files that must survive a reboot even where `data_dir` is tmpfs: `infra-monitor.json`, `infra-secrets.json`, `tls_pins.json`. Created `0700`. RAM-booted device profiles set it to `/etc/sctl/state`. Env: `SCTL_STATE_DIR`. |
| `journal_enabled` | `true` | Enable session output journaling to disk. |
| `openwrt_persistent_logs` | `false` | On OpenWrt, configure logd to persist system logs to overlay. Off by default to spare flash on small embedded devices. |
| `openwrt_persistent_log_size_kb` | `128` | Persistent OpenWrt log size in KiB when the above is `true`. |
| `journal_fsync_interval_ms` | `5000` | Batch fsync interval, milliseconds (`0` = fsync every write). |
| `journal_max_age_hours` | `72` | Auto-delete journals older than this many hours. |
| `playbooks_dir` | `"/etc/sctl/playbooks"` | Directory containing playbook markdown files. Env: `SCTL_PLAYBOOKS_DIR`. |
| `activity_log_max_entries` | `200` | Maximum entries in the in-memory activity log ring buffer. |
| `exec_result_cache_size` | `100` | Maximum cached full exec results kept in memory (backs `GET /api/activity/{id}/result`). |
| `default_terminal_rows` | `24` | Default PTY rows when a session does not specify. |
| `default_terminal_cols` | `80` | Default PTY columns when a session does not specify. |
| `max_concurrent_transfers` | `4` | Maximum concurrent STP (gawdxfer) transfers. |
| `transfer_chunk_size` | `262144` (256 KiB) | STP chunk size in bytes. |
| `transfer_max_file_size` | `1073741824` (1 GiB) | Maximum file size for STP transfers, bytes. |
| `transfer_stale_timeout_secs` | `3600` | Seconds after which an inactive transfer is swept. |

## `[auth]`

Authentication (`AuthConfig`).

| Key | Default | Description |
|-----|---------|-------------|
| `api_key` | `"change-me"` | Pre-shared Bearer token for every authenticated route. The default triggers a startup warning. Env: `SCTL_API_KEY` (preferred over storing the key in the file). |

## `[shell]`

Defaults used when requests don't specify overrides (`ShellConfig`).

| Key | Default | Description |
|-----|---------|-------------|
| `default_shell` | `"/bin/sh"` | Shell binary for exec and sessions. |
| `default_working_dir` | `"/"` | Working directory for exec and sessions. |

## `[device]`

Device identity (`DeviceConfig`).

| Key | Default | Description |
|-----|---------|-------------|
| `serial` | `"SCTL-0000-DEV-001"` | Unique device serial, reported in `/api/info` and used as the tunnel registration identity. Env: `SCTL_DEVICE_SERIAL`. |

## `[logging]`

Logging (`LoggingConfig`).

| Key | Default | Description |
|-----|---------|-------------|
| `level` | `"info"` | Log level: `off`, `error`, `warn`, `info`, `debug`, `trace`. Overridden by `RUST_LOG`. Target-specific `RUST_LOG` directives are parsed for their most verbose level but are not target-filtered. |

## `[supervisor]`

Settings for `sctl supervise` (`SupervisorConfig`). The crash-loop
threshold itself (3 crashes in 180 s → safe mode) is compiled in — see
[safe-mode.md](safe-mode.md).

| Key | Default | Description |
|-----|---------|-------------|
| `max_backoff` | `60` | Maximum seconds between restart attempts (exponential ramp; safe mode stretches this to a fixed 300 s). |
| `stable_threshold` | `60` | Seconds of child uptime before the backoff resets to 1 s. |

## `[tunnel]`

Tunnel configuration (`TunnelConfig`) — omit the section entirely to
disable tunneling. Two mutually exclusive modes:

- **Relay mode** (`relay = true`): this instance is a relay server; devices
  connect inbound, clients reach them at `/d/{serial}/api/*`.
- **Client mode** (`url` set, `relay = false`): this instance dials out to
  a relay and stays registered.

| Key | Default | Description |
|-----|---------|-------------|
| `relay` | `false` | Run as a tunnel relay. |
| `tunnel_key` | *(required)* | Shared secret for device↔relay authentication. Min 8 chars in relay mode. |
| `url` | *(none)* | Relay URL for client mode, e.g. `wss://relay.example.com/api/tunnel/register`. |
| `reconnect_delay_secs` | `2` | Initial reconnect backoff (client mode). |
| `reconnect_max_delay_secs` | `30` | Maximum reconnect backoff (client mode). |
| `heartbeat_interval_secs` | `5` | Ping interval (client mode). **Clamped to `[1, 15]` at use** — on LTE/CGNAT paths, idle periods much above ~15 s get culled by the network long before the logical tunnel timeout, so a stale config cannot silently disable keepalives. |
| `heartbeat_timeout_secs` | `45` | Seconds without a heartbeat before the relay declares a device dead (relay mode). |
| `tunnel_proxy_timeout_secs` | `60` | Default proxy request timeout (relay mode); the source of relay `504 TIMEOUT` errors. |
| `bind_address` | *(none)* | Local address **or interface name** to bind outbound tunnel connections to (client mode). Interface names are resolved to their current IPv4 on each connect attempt (survives DHCP/carrier changes) and get `SO_BINDTODEVICE`; an IP literal sets only the source address and does **not** pin egress to the interface. Either way the tunnel stays where it is bound and does not follow the kernel's route changes. |
| `relay_route` | `"off"` | Who keeps the host route to the relay (client mode). `"off"` leaves it to the system. `"follow_default"` makes sctl keep one route to the relay on the lowest-metric uplink that reaches it. `"prefer"` does the same with the uplinks in `relay_route_prefer` taken first, in that order, whatever their metrics; see below. Refused together with `bind_address`. |
| `relay_route_prefer` | `[]` | With `relay_route = "prefer"` only: the interfaces to prefer for the relay route, in order, e.g. `["eth0", "usb0"]`. Required, non-empty and distinct with `"prefer"`; refused with any other mode. |
| `tls_ca_file` | *(none)* | PEM file with additional root CAs for `wss://` client connections. Public webpki roots stay enabled. |
| `tls_server_cert_sha256` | *(none)* | SHA-256 pin (lowercase hex or colon-separated) for the relay's leaf certificate DER; checked after normal rustls validation. |

Routes sctl installs on the device carry route protocol `83`, a value the
kernel's list leaves unassigned, so `ip route show proto 83` lists exactly
those. sctl deletes a route only by naming that protocol, so it never
removes a route that netifd, DHCP or an operator added.

### `relay_route`

With `relay_route = "follow_default"` or `"prefer"`, sctl owns one route:
`<relay>/32 via <gw> dev <uplink> metric 0 proto 83`, where `<relay>` is the
address the tunnel connected to, and `<gw>` the gateway of the uplink's
default route. When that default route declares its gateway `onlink` (a
gateway outside the uplink's prefix), sctl's route does too; the kernel
refuses it otherwise. At metric 0 it outranks the pin netifd adds for the
WireGuard endpoint (the same address), which is left in place as the
fallback. The route stays when sctl exits, because WireGuard uses it too.

- **Which uplink.** With `follow_default`, the lowest-metric default route
  whose interface is not suspect: netifd's metrics already rank the wire
  above LTE. With `prefer`, the first interface in `relay_route_prefer` that
  holds a default route and is not suspect, whatever its metric; when none of
  the listed ones does, the lowest-metric rule over the unlisted ones. A
  suspect uplink is never chosen, listed or not. The choice is made again on
  every network change the kernel reports (after a quiet second) and after
  every registration with the relay.
- **When the metrics are wrong.** On the WE826 the vendor's dial script
  installs LTE's default route at metric 0 on every dial, and the BPI keeps
  LTE at the lower metric too, so `follow_default` would keep the relay on
  the SIM while a wire is up. `relay_route = "prefer"` with
  `relay_route_prefer = ["eth0", "usb0"]` names the wire first: the relay
  rides the wire while the wire reaches the relay, and the probe and suspect
  machinery below still moves it to LTE when the wire cannot, and back when
  it can again. Whatever the default route does for passenger traffic is not
  sctl's business; only the relay's `/32` moves.
- **Only an uplink that answers.** Before the route moves to an uplink the
  tunnel is not already on, sctl probes the relay over that interface
  alone (`SO_BINDTODEVICE`, as `bind_address` does). Over `wss://` the probe
  is the tunnel's own TLS handshake, with its server name, `tls_ca_file` and
  `tls_server_cert_sha256`, closed straight after (no WebSocket upgrade, no
  key sent), so an uplink that passes TCP but breaks TLS (SSL inspection, a
  captive portal) does not answer. Over `ws://` it is a TCP connect. An
  uplink that does not answer is marked suspect.
- **Link up, internet dead.** No kernel event fires, so the tunnel's own
  failures are the signal: a dial nothing answers, a handshake that breaks
  or stalls, a pong timeout, a read or write error. After two in a row on
  the uplink the route uses, that uplink and every other one holding a
  default route are asked in the same round, all at once. The route moves
  only when its own uplink does not answer while another does, to the first
  of those in the same order (the `relay_route_prefer` list, then metric),
  and the tunnel then follows the route (see the `rehome` event in the HTTP
  API).
- **The relay is down.** Then no uplink answers, the route's own included,
  so nothing moves and nothing becomes suspect: a relay restart never sends
  a unit to LTE. When the route's own uplink answers, the failures were the
  relay's, and the route stays too.
- **When the relay's name stops resolving.** The resolver's upstream servers
  are usually reached through the same dead uplink, so the tunnel dials the
  relay's known address when a lookup fails: where it last reached the relay
  or, after sctl restarts, where sctl's route points. That dial failing
  counts as a failure of the path, and it reaches the relay once the route
  has moved. A restarted sctl keeps the route it finds and counts those
  failures before any registration.
- **Coming back.** A suspect uplink is asked again at once when the kernel
  reports a change on it (link, address or default route: a replug, a new
  DHCP lease), and is no longer suspect as soon as the tunnel registers
  with the relay over it. Otherwise, and only while it is suspect, it is
  probed on its own schedule, 2, 5 and 10 minutes after it became suspect
  and then every 15 minutes, and it gets the route back after two good
  probes in a row. With nothing suspect, no timer runs.
- **Flapping.** An uplink that fails again within 10 minutes of getting the
  route back waits twice as long before each probe as it did last time,
  never more than an hour: after one relapse it is asked after 4, 10 and 20
  minutes, then every 30; after two, after 8, 20 and 40 minutes, then every
  hour. Once it keeps the route for an hour, it starts over at 2 minutes.
- **Someone else's route.** A `/32` to the relay at metric 0 that sctl did
  not install (a hand-added pin, an older hotplug) is never replaced: sctl
  leaves the relay to it, records a `relay_route` event saying so, and takes
  over once that route is gone.
- **Reporting.** `/api/health` shows `tunnel.relay_route` (with the
  `prefer` list when one is set), and every move is a `relay_route` tunnel
  event such as `eth1 -> wwan0 (eth1: no answer from the relay)`.

It is opt-in because some units route the relay on purpose: the BPI units
keep it on LTE for out-of-band access, and a WE826 that must stay on one
uplink pins the tunnel with `bind_address` instead.

The answer to a probe arrives on the uplink it left by, so reverse-path
filtering on the uplinks must be loose (`rp_filter = 2`) or off, as
`bind_address` already requires: with strict filtering (1) an uplink the
route does not use never answers, and once suspect it stays suspect.
`/api/health` lists the uplinks where it is strict in
`tunnel.relay_route.rp_filter_strict`, and the XE300 packaging sets it loose
(`devices/xe300/README.md`).

## `[comms]`

External comms provider plugin (`CommsConfig`) — the C-ABI shared library
that owns the modem serial port. Omit the section on relay/VPS/server-only
installs. `[gps]` and `[lte]` refuse to start without an enabled `[comms]`.

**Unknown keys are tolerated** (no `deny_unknown_fields`): units
provisioned in the process-helper era carry a now-removed
`[comms] command = "..."` key, and rejecting it would panic `Config::load`
and take down the entire management plane on a unit that is otherwise
fine. Unknown keys are ignored.

| Key | Default | Description |
|-----|---------|-------------|
| `provider` | `"quectel-at"` | Provider name. `"none"` disables external comms (equivalent to omitting the section). |
| `library` | *(derived)* | Shared library path. Defaults to `/usr/lib/sctl/comms/libsctl_comms_<provider>.so` (dashes→underscores). |
| `device` | *(none)* | Provider device hint, e.g. `/dev/ttyUSB2`. Takes precedence over `[lte].device` / `[gps].device`; autodetection is preferred when available. |
| `startup_timeout_secs` | `15` | Seconds to wait for provider startup/open. |
| `request_timeout_secs` | `20` | Seconds to wait for each provider request. |

## `[gps]`

GPS/location polling through the active comms provider (`GpsConfig`).
Exposes `/api/gps` and enriches the health endpoint.

| Key | Default | Description |
|-----|---------|-------------|
| `device` | *(none)* | AT serial device hint. `[comms].device` takes precedence; the Quectel provider autodetects via sysfs. |
| `poll_interval_secs` | `30` | Seconds between GPS polls. |
| `history_size` | `100` | Maximum GPS fix history entries. |
| `auto_enable` | `true` | Auto-enable the GNSS engine on startup. |

## `[lte]`

LTE/cellular signal monitoring and the autonomous recovery watchdog
(`LteConfig`). Exposes `/api/lte` and enriches `/api/info`.

| Key | Default | Description |
|-----|---------|-------------|
| `device` | *(none)* | AT serial device hint. `[comms].device` takes precedence; the Quectel provider autodetects via sysfs. |
| `poll_interval_secs` | `60` | Seconds between LTE signal polls. |
| `watchdog` | `true` | Enable the LTE watchdog for autonomous modem recovery. It only acts when the tunnel is down and grace has elapsed, and diagnoses from kernel truth (interface IPv4 + interface-bound ping). |
| `interface` | `"wwan0"` | Network interface name for the LTE modem (IP checks, speed tests, interface-bound pings). |
| `speed_test_url` | *(none)* | Download speed-test URL for band scans. |
| `speed_test_upload_url` | *(none)* | Upload speed-test URL for band scans (server must accept POST data). |
| `reachability_host` | *(derived)* | Host pinged for internet reachability before modem escalation. Default: the relay host from `tunnel.url`, else `8.8.8.8`. |
| `interface_restart_cmd` | *(derived)* | Custom command to restart the LTE interface, overriding auto-detection. E.g. `"ifdown wwan && sleep 2 && ifup wwan"`. |
| `watchdog_grace_secs` | `120` | Seconds the tunnel must be down before the watchdog acts at all. Higher values ride out natural roaming handovers. |
| `max_escalation_level` | `3` | USB power-cycle is opt-in: the watchdog only deauthorizes/reauthorizes the modem's USB port when this is `>= 4`. Re-enumeration can shift the `ttyUSB*` minor and is not idempotent, so it stays last-resort. Manual `POST /api/lte/usb_cycle` ignores this. |
| `usb_cycle_evidence` | see below | Evidence gates for the *automatic* USB power-cycle path. |
| `notregistered_grace_secs` | `180` | Seconds of `NotRegistered` tolerated before it is treated as actionable. |

### `[lte.usb_cycle_evidence]`

Evidence requirements gating an automatic USB power-cycle even when
`max_escalation_level >= 4` (`UsbCycleEvidence`). Manual
`POST /api/lte/usb_cycle` ignores these. The cycle additionally requires
the modem to still be present in sysfs.

| Key | Default | Description |
|-----|---------|-------------|
| `min_sustained_secs` | `600` | Minimum sustained seconds of the qualifying symptom before an automatic USB cycle is allowed. |
