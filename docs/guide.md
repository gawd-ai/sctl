# sctl Guide

The complete guide to deploying, configuring, and using sctl.

## How sctl Works

sctl is a three-component system for remote device control:

```
AI prompt
  -> Claude Code / AI agent
    -> mcp-sctl (MCP proxy, runs on your machine)
      -> JSON-RPC 2.0 over stdio
        -> HTTP/WebSocket request
          -> sctl server (runs on the device)
            -> shell / PTY / filesystem
              -> output
          <- WebSocket stream / HTTP response
        <- local output buffer (instant reads)
      <- MCP tool result
    <- AI sees the output and decides next action
```

**Three access modes:**

1. **MCP** (AI agents) -- mcp-sctl translates tool calls into API requests. Sessions are buffered locally for zero-latency reads.
2. **HTTP/WS API** (direct) -- any client can call the REST endpoints or open a WebSocket for interactive sessions.
3. **sctlin web UI** -- Svelte 5 terminal component library. Embeddable or standalone.

**Persistent sessions** are the core differentiator. When you start a session with `persistent: true`, the shell process lives on the device. Output is buffered in a ring buffer with monotonically increasing sequence numbers. If the network drops, the session keeps running. When you reconnect, `session.attach` replays everything you missed. No output is lost (unless the ring buffer wraps).

## Deployment

### Any Linux

The server is a single static binary. No runtime dependencies.

```bash
# Build
cd server && cargo build --release

# Run
SCTL_API_KEY=your-secret-key ./target/release/sctl

# Or with a config file
./target/release/sctl serve --config sctl.toml
```

Environment variables for quick setup:

| Variable | Default | Description |
|----------|---------|-------------|
| `SCTL_API_KEY` | -- | **Required.** Pre-shared auth key |
| `SCTL_LISTEN` | `0.0.0.0:1337` | Bind address |
| `SCTL_DEVICE_SERIAL` | `SCTL-0000-DEV-001` | Device serial for identification |
| `SCTL_DATA_DIR` | `/var/lib/sctl` | Persistent data (journals) |
| `RUST_LOG` | `info` | Log level filter |

For full TOML configuration, see [sctl.toml.example](../server/sctl.toml.example).

systemd example:

```ini
[Unit]
Description=sctl device control server
After=network.target

[Service]
ExecStart=/usr/local/bin/sctl serve --config /etc/sctl/sctl.toml
Environment=SCTL_API_KEY=your-secret-key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### Cross-Compilation

sctl uses [cross](https://github.com/cross-rs/cross) for static musl builds:

```bash
# Install cross (Docker-based cross-compiler)
cargo install cross --git https://github.com/cross-rs/cross

# ARM (Raspberry Pi, OpenWrt ARM routers)
cross build --release --target armv7-unknown-linux-musleabihf

# RISC-V (BPI-RV2, OpenWrt RISC-V routers)
cross build --release --target riscv64gc-unknown-linux-musl

# x86_64 (VPS, containers)
cross build --release --target x86_64-unknown-linux-musl
```

Or via the Makefile in `server/`:

```bash
make build-arm     # ARM build
make build-riscv   # RISC-V build
```

### Device Management with rundev.sh

`rundev.sh` handles discovery, cross-compilation, deployment, and upgrades:

```bash
# Discover and register a device (probes via SSH for arch, serial, api_key)
./rundev.sh device add mydevice 192.168.1.1

# List all devices with live health checks
./rundev.sh device ls

# Full deploy: cross-compile + upload binary + config + init script
./rundev.sh device deploy mydevice

# Binary-only upgrade: cross-compile + stop + upload + start
./rundev.sh device upgrade mydevice

# Remove a device from config
./rundev.sh device rm mydevice
```

Supported architectures: `riscv64`, `armv7l`, `aarch64`, `x86_64`.

### Supervisor Mode

sctl includes a built-in supervisor with exponential backoff restart:

```bash
./sctl supervise --config sctl.toml
```

The supervisor restarts the server on crash, with configurable backoff (`max_backoff`, `stable_threshold` in `[supervisor]`). If the server runs longer than `stable_threshold` seconds, the backoff resets.

### OpenWrt / Embedded

A [procd init script](../server/files/sctl.init) is included for OpenWrt service management with auto-restart. Deploy with `rundev.sh device deploy` or the Makefile:

```bash
cd server
make deploy HOST=192.168.1.1         # ARM
make deploy-riscv HOST=192.168.1.1   # RISC-V
```

Flash storage considerations: set `journal_enabled = false` or use a tmpfs `data_dir` to avoid flash wear from output journaling.

## Multi-Device Fleet

mcp-sctl supports managing multiple devices from a single MCP server.

### Config format

```json
{
  "config_version": 2,
  "devices": {
    "router-1": {
      "url": "http://192.168.1.1:1337",
      "api_key": "key-for-router-1",
      "playbooks_dir": "/etc/sctl/playbooks"
    },
    "router-2": {
      "url": "https://relay.example.com/d/SCTL-0002",
      "api_key": "key-for-router-2"
    }
  },
  "default_device": "router-1"
}
```

Devices can mix direct URLs and relay URLs. The AI (or any caller) passes `device: "router-2"` to target a specific device, or omits it to use the default.

### Hot-reload

mcp-sctl checks the config file's mtime before each device lookup. Edit the JSON file and changes take effect immediately -- no restart needed. WebSocket connections for changed or removed devices are dropped automatically.

### Metadata fields

The config may also contain metadata fields (`host`, `serial`, `arch`, `sctl_version`, `added_at`) used by `rundev.sh device` commands. mcp-sctl ignores unknown fields, so they're safe to include.

## Reverse Tunnel

sctl includes a built-in reverse tunnel for devices behind CGNAT (LTE/5G) that can't accept inbound connections.

### When you need it

- Device is on LTE/5G with carrier-grade NAT
- Device is behind a firewall you don't control
- No port forwarding available
- You want a single public URL for all your devices

### Architecture

```
Device (behind CGNAT)                 Relay (public VPS)               Clients
 +--------+                           +-------------+                  +---------+
 | sctl   |--- outbound WS ---------> | sctl        | <--- HTTP/WS -- | mcp-sctl|
 | server |   (registers serial)      | (relay mode)|   (same API)    | sctlin  |
 +--------+                           +-------------+                  +---------+
```

The device initiates an outbound WebSocket to the relay and registers with its serial number. The relay learns the device's API key during registration. Clients connect to the relay using the same sctl API -- just a different base URL.

### Relay setup

Run sctl in relay mode on a VPS or any publicly reachable host:

```toml
# sctl.toml on the relay VPS
[server]
listen = "0.0.0.0:1337"

[auth]
api_key = "relay-admin-key"

[tunnel]
relay = true
tunnel_key = "shared-secret-between-relay-and-devices"
heartbeat_timeout_secs = 45
tunnel_proxy_timeout_secs = 60
```

Put a TLS-terminating reverse proxy in front (Caddy, nginx, or cloudflared):

```
# Caddy example
relay.example.com {
    reverse_proxy localhost:1337
}
```

### Device setup

On the CGNAT device, configure tunnel client mode:

```toml
# sctl.toml on the device
[device]
serial = "MY-DEVICE-001"

[auth]
api_key = "device-specific-key"

[tunnel]
tunnel_key = "shared-secret-between-relay-and-devices"
url = "wss://relay.example.com/api/tunnel/register"
```

### How clients connect

Clients use the relay URL with the device serial:

- **Direct:** `http://192.168.1.1:1337`
- **Via relay:** `https://relay.example.com/d/MY-DEVICE-001`

No changes to mcp-sctl or sctlin -- just a different `url` in the config.

In your mcp-sctl config:
```json
{
  "devices": {
    "my-device": {
      "url": "https://relay.example.com/d/MY-DEVICE-001",
      "api_key": "device-specific-key"
    }
  }
}
```

All operations (exec, sessions, files, playbooks, GPS) work transparently through the tunnel.

### Dual-path pattern

For devices accessible both on LAN and via LTE, configure two entries pointing to the same device:

```json
{
  "devices": {
    "router-lan": {
      "url": "http://192.168.1.1:1337",
      "api_key": "device-key"
    },
    "router-lte": {
      "url": "https://relay.example.com/d/SCTL-0001",
      "api_key": "device-key"
    }
  },
  "default_device": "router-lan"
}
```

Use `router-lan` when on the local network, `router-lte` when remote.

### LTE interface binding

Force tunnel traffic over the LTE modem by binding to its interface:

```toml
[tunnel]
bind_address = "wwan0"    # Interface name or IP address
```

This ensures the tunnel uses the cellular connection even if the device has other network paths.

### Health and resilience

The tunnel includes built-in resilience features:

- **Heartbeat** -- configurable ping interval with RTT tracking (median and p95)
- **Auto-reconnect** -- exponential backoff from `reconnect_delay_secs` to `reconnect_max_delay_secs`
- **Flap detection** -- 3 connections in under 30 seconds triggers a 60-second cooldown backoff
- **Writer channel monitoring** -- warns at 75% capacity, prevents backpressure stalls
- **Health probe** -- relay periodically probes device health; unhealthy state resets on reconnect

Health metrics are exposed in the `/api/health` response:

```json
{
  "tunnel": {
    "connected": true,
    "reconnects": 2,
    "uptime_secs": 3600,
    "rtt_median_ms": 45,
    "rtt_p95_ms": 120,
    "dropped_outbound": 0
  }
}
```

### Dev testing

```bash
# Start a local relay + connect devices through it
./rundev.sh tunnel
```

This builds everything, starts a local relay, connects registered devices via SSH, and rewrites the MCP config so all traffic flows through the relay.

## Playbooks

Playbooks are shell scripts with structured metadata, stored as markdown files on devices. They are automatically discovered and exposed as MCP tools.

### Format

A playbook is a markdown file with YAML frontmatter and a fenced `sh` code block:

~~~markdown
---
name: my-health-check
description: Check system health with configurable thresholds
params:
  disk_threshold:
    type: string
    description: Disk usage percentage threshold for alerts
    default: "90"
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
DISK_THRESHOLD="{{disk_threshold}}"
VERBOSITY="{{verbosity}}"

echo "Checking disk usage (threshold: ${DISK_THRESHOLD}%)..."
df -h
```
~~~

**Frontmatter fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Unique identifier (becomes the MCP tool name with `pb_` prefix) |
| `description` | yes | Human-readable description (shown to AI agents) |
| `params` | no | Map of parameter names to `{type, description, default?, enum?}` |

**Template substitution:** `{{param_name}}` in the script body is replaced with the parameter value at execution time.

### Auto-discovery

sctl watches the `playbooks_dir` directory (default `/etc/sctl/playbooks`). Any `.md` file with valid frontmatter is automatically exposed as:

- A REST endpoint: `GET /api/playbooks/{name}`
- An MCP tool: `pb_{name}` (via mcp-sctl)

mcp-sctl fetches the playbook list on first request and caches it per-device. The AI sees playbooks as regular tools with typed parameters.

### Built-in library

sctl ships with 8 playbooks covering common operations:

| Playbook | MCP Tool | Description |
|----------|----------|-------------|
| `linux/diagnostics` | `pb_linux-diagnostics` | Comprehensive Linux system diagnostics |
| `linux/health-check` | `pb_linux-health-check` | Disk, CPU, memory, zombies, NTP, failed services |
| `linux/security-hardening` | `pb_linux-security-hardening` | SSH, firewall, users, permissions audit |
| `openwrt/diagnostics` | `pb_openwrt-diagnostics` | OpenWrt-specific system diagnostics |
| `openwrt/health-check` | `pb_openwrt-health-check` | OpenWrt health monitoring |
| `openwrt/network-setup` | `pb_openwrt-network-setup` | Network interface configuration |
| `openwrt/security-hardening` | `pb_openwrt-security-hardening` | OpenWrt security audit and hardening |
| `openwrt/network-mode` | `pb_network-mode` | Configure ethernet port roles (router/switch/hybrid) |

Deploy them by copying to the device's `playbooks_dir`:

```bash
scp playbooks/linux/*.md device:/etc/sctl/playbooks/
```

Or use `rundev.sh device deploy` which includes playbooks automatically.

### Writing custom playbooks

1. Create a `.md` file with YAML frontmatter and a fenced `sh` code block
2. Place it in the device's `playbooks_dir`
3. It's immediately available as an MCP tool and REST endpoint

Tips:
- Use `enum` for parameters with fixed choices -- AI agents will present them as options
- Provide sensible `default` values so the playbook works without configuration
- Scripts run as the sctl server's user (usually root on embedded devices)
- Use `#!/bin/sh` for portability (OpenWrt uses `/bin/ash`, not bash)

### Managing via API

Playbooks can be created, updated, and deleted remotely:

```bash
# Upload a playbook
curl -X PUT -H "Authorization: Bearer $KEY" \
  -H "Content-Type: text/markdown" \
  --data-binary @my-playbook.md \
  http://device:1337/api/playbooks/my-playbook

# Delete a playbook
curl -X DELETE -H "Authorization: Bearer $KEY" \
  http://device:1337/api/playbooks/my-playbook
```

Or via MCP: `playbook_put` with the full markdown content.

## GPS & LTE Monitoring

sctl can monitor GPS location and LTE signal quality from Quectel modems (tested with EC25).

### Hardware requirements

- Quectel modem with AT command support (EC25, RM500Q, etc.)
- Serial port access (typically `/dev/ttyUSB2` for AT commands)
- Antennas: MAIN (LTE TX/RX), DIV (LTE RX diversity), GNSS (GPS)

### GPS configuration

```toml
[gps]
device = "/dev/ttyUSB2"       # Serial device for AT commands
poll_interval_secs = 30       # Seconds between GPS polls
history_size = 100             # Maximum fix history entries
auto_enable = true             # Auto-enable GNSS engine on startup
```

### GPS data

The `GET /api/gps` endpoint returns:

- **Current fix:** latitude, longitude, altitude, speed, course, satellites, HDOP, fix type (2D/3D)
- **History:** configurable ring buffer of recent fixes
- **Status:** active/inactive, fix age, total fixes, error count

GPS fixes are also broadcast over WebSocket as `gps.fix` messages.

MCP tool: `device_gps` returns the same data.

### LTE configuration

```toml
[lte]
device = "/dev/ttyUSB2"       # Serial device for AT commands
poll_interval_secs = 60       # Seconds between signal polls
watchdog = true                # Auto-recovery when signal or tunnel drops
interface = "wwan0"            # Network interface for IP checks
```

### LTE data

The `GET /api/info` response includes LTE metrics when configured:

- **Signal:** RSSI, RSRP, RSRQ, SINR, signal bars (1-5)
- **Cell:** band, operator, technology (LTE/WCDMA), cell ID
- **Modem:** model, firmware, IMEI, ICCID
- **Band history:** recent band transitions with timestamps
- **Neighbor cells:** visible cells and their signal strength

LTE signal updates are broadcast over WebSocket as `lte.signal` messages.

### Band control

Control which LTE bands the modem uses:

```bash
# Set allowed bands via the API
curl -X POST -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -d '{"bands": ["B4", "B12", "B13"]}' \
  http://device:1337/api/lte/bands
```

Band scanning tests each band's throughput and selects the best configuration.

### LTE watchdog

When `watchdog = true`, the LTE subsystem includes autonomous recovery:

- Detects modem unresponsiveness and triggers resets
- Restores "safe bands" configuration after recovery
- **Tunnel-aware:** skips AT commands while the tunnel is connected to avoid disrupting the data path (Quectel modems can't handle concurrent AT commands and QMI data reliably)
- On-demand polling via API requests when the regular polling is suppressed

## AI Collaboration

sctl supports real-time AI/human collaboration on sessions.

### Session-level AI control

Each session has an `allow_ai` flag. When `false`, mcp-sctl cannot send input to the session -- only the web UI can. This enables handoff patterns:

1. AI starts a session and begins debugging
2. Human sees something interesting in sctlin, disables AI input
3. Human takes over, types commands manually
4. Human re-enables AI, which continues from where it left off

Toggle via MCP (`session_allow_ai`) or the sctlin UI.

### AI status tracking

mcp-sctl automatically reports what it's doing:

- **`working`** -- `true` when AI is actively using the session
- **`activity`** -- `"read"` or `"write"` (set automatically by mcp-sctl for `session_read`, `session_exec`, `session_send`)
- **`message`** -- optional human-readable status

The status auto-clears after 60 seconds of inactivity.

### sctlin integration

The sctlin web UI shows AI status in real time:

- Working indicator on session tabs
- Activity type (reading/writing) visible
- Human can see what the AI is doing and take over at any point

### Best practices

- Use `device_exec` for one-shot commands (simpler, no session overhead)
- Use sessions for interactive work, multi-step debugging, or long-running processes
- Use `session_exec_wait` for fire-and-wait commands within a session (returns output + exit code in one call)
- Name your sessions (`name` parameter) so humans can identify them in the UI

## sctlin Web UI

sctlin is a Svelte 5 component library with a standalone web app for terminal access, device monitoring, and playbook execution.

### Running standalone

```bash
cd web
npm install
npm run dev       # Development server
npm run build     # Production build
```

The standalone app stores server connections in browser localStorage. Add servers via the UI.

### Deploying to a VPS

Build and deploy alongside your relay:

```bash
# Build the web app
cd web && npm run build && npm run package

# Or use rundev.sh
./rundev.sh relay sctlin
```

Serve the static files with your reverse proxy (Caddy, nginx).

### Connecting to devices

In the sctlin UI, add servers with their connection details:

- **Direct:** `ws://192.168.1.1:1337/api/ws` + device API key
- **Via relay:** `wss://relay.example.com/d/MY-DEVICE-001/api/ws` + device API key

sctlin supports multiple simultaneous server connections with a sidebar for switching between them.

### Embedding in your app

sctlin is also a component library (`npm install sctlin`). See the [web README](../web/README.md) for component API, widgets, and integration examples.

## Troubleshooting

### Device not responding

- Verify the server is running: `curl http://device:1337/api/health`
- Check firewall rules allow port 1337
- Verify the API key matches: a `403` response means wrong key, `401` means missing header
- For tunnel devices: check relay health at `https://relay.example.com/api/health`

### Sessions disappearing

- Non-persistent sessions are killed on WebSocket disconnect (this is the default for direct WS, but mcp-sctl defaults to `persistent: true`)
- Persistent sessions with `idle_timeout` are cleaned up after inactivity while detached
- Server restart kills all sessions (output journals persist if journaling is enabled)

### Tunnel unstable

- Check `GET /api/health` -- the `tunnel` section shows RTT, reconnects, and recent events
- High RTT or frequent reconnects may indicate network issues
- If using `bind_address`, verify the LTE interface has an IP
- Flap detection triggers a 60-second backoff after 3 rapid reconnections

### PTY not working

- Set `pty: true` when starting the session
- Verify the shell binary exists on the device (e.g. `/bin/ash` on OpenWrt, not `/bin/bash`)
- PTY allocation requires the device to have `/dev/ptmx` (standard on Linux)

### MCP tools missing

- Verify mcp-sctl is registered: `claude mcp list`
- Check config path: `mcp-sctl --config /path/to/devices.json`
- Test device connectivity: `device_health` tool
- For playbook tools (`pb_*`): verify playbooks exist in the device's `playbooks_dir`

### File operations failing

- Paths must be absolute (start with `/`)
- No `..` components or null bytes allowed (path traversal protection)
- File size limited to `max_file_size` (default 50 MB)
- Write operations use temp-file-then-rename -- the parent directory must be writable
