<p align="center">
  <img src="../sctl-logo.png" alt="sctl" width="120" />
</p>

# sctl

Remote device control server for AI agents.

## Overview

sctl exposes HTTP and WebSocket APIs that let an AI agent (or any authenticated client) execute commands, manage interactive shell sessions with full PTY support, read/write files, and query device status on Linux devices -- all protected by a pre-shared API key.

Works on any Linux system -- x86_64 servers, ARM single-board computers, RISC-V routers, and more.

```
┌──────────────┐         HTTPS / WSS          ┌──────────────────────┐
│              │ ◄──────────────────────────► │  sctl                │
│  AI Agent /  │   Bearer token auth          │  ┌────────────────┐  │
│  Dashboard   │                              │  │ HTTP routes    │  │
│              │   POST /api/exec             │  │  exec, files,  │  │
│              │   GET  /api/info             │  │  info, health  │  │
│              │   GET  /api/ws               │  ├────────────────┤  │
│              │                              │  │ WebSocket      │  │
│              │ ◄─ session.stdout ────────── │  │  sessions mgr  │  │
│              │ ── session.exec ───────────► │  │  stdin/out/err │  │
└──────────────┘                              │  └────────────────┘  │
                                              │       ▼              │
                                              │  ┌────────────────┐  │
                                              │  │ /bin/sh -c ... │  │
                                              │  └────────────────┘  │
                                              └──────────────────────┘
                                                   Linux device
```

## Quick Start

```bash
# Build
cargo build --release

# Configure
export SCTL_API_KEY="your-secret-key"

# Run
./target/release/sctl
```

sctl listens on `0.0.0.0:1337` by default. Test it:

```bash
curl http://localhost:1337/api/health
# {"status":"ok","uptime_secs":5,"version":"0.4.0","sessions":0,...}

curl -H "Authorization: Bearer your-secret-key" http://localhost:1337/api/exec \
  -H "Content-Type: application/json" \
  -d '{"command":"uname -a"}'
```

## Configuration

sctl loads configuration in order of precedence (highest wins):

1. **Environment variables** -- `SCTL_API_KEY`, `SCTL_LISTEN`, `SCTL_DEVICE_SERIAL`
2. **Config file** -- `--config <path>` flag, or `sctl.toml` in CWD
3. **Compiled defaults**

### TOML reference

```toml
[server]
listen = "0.0.0.0:1337"            # Bind address (env: SCTL_LISTEN)
max_connections = 10                # (not currently enforced)
max_sessions = 20                   # Concurrent WebSocket shell sessions
session_buffer_size = 1000          # Max output entries per session ring buffer
exec_timeout_ms = 30000             # Default exec timeout in ms (30s)
max_batch_size = 20                 # Max commands per batch request
max_file_size = 52428800            # Max file read/write/delete size (50 MB)
data_dir = "/var/lib/sctl"          # Persistent data (journals, etc)
journal_enabled = true              # Disk-backed output journaling
journal_fsync_interval_ms = 5000    # Batch fsync interval (0 = every write)
journal_max_age_hours = 72          # Auto-delete journals older than this
default_terminal_rows = 24          # Default PTY rows
default_terminal_cols = 80          # Default PTY columns

[auth]
api_key = "change-me"               # Override with SCTL_API_KEY

[shell]
default_shell = "/bin/sh"           # Shell binary for exec and sessions
default_working_dir = "/"           # Default working directory

[device]
serial = "SCTL-0000-DEV-001"       # Device serial (env: SCTL_DEVICE_SERIAL)

[logging]
level = "info"                      # Log filter (env: RUST_LOG)

[supervisor]
max_backoff = 60                    # Max seconds between restart attempts
stable_threshold = 60               # Seconds of uptime before resetting backoff

# Optional -- omit [tunnel] entirely to disable
[tunnel]
relay = false                       # true = relay mode, false = client mode
tunnel_key = "shared-secret"        # Device<->relay auth
url = "wss://relay.example.com/api/tunnel/register"  # Client mode only
reconnect_delay_secs = 2            # Client mode initial backoff
reconnect_max_delay_secs = 30       # Client mode max backoff
heartbeat_interval_secs = 15        # Client mode ping interval
bind_address = "wwan0"              # Client mode: bind to interface or IP (LTE failover)
heartbeat_timeout_secs = 45         # Relay mode: seconds before device eviction
tunnel_proxy_timeout_secs = 60      # Relay mode: proxy request timeout

# Optional — GPS location tracking via Quectel modem GNSS
[gps]
device = "/dev/ttyUSB2"             # Serial device for AT commands
poll_interval_secs = 30             # Seconds between GPS polls
history_size = 100                  # Maximum fix history entries
auto_enable = true                  # Auto-enable GNSS engine on startup

# Optional — LTE signal monitoring via Quectel modem AT commands
[lte]
device = "/dev/ttyUSB2"             # Serial device for AT commands
poll_interval_secs = 60             # Seconds between signal polls
```

## API Reference

All endpoints except `/api/health` require `Authorization: Bearer <key>`.

| Method | Path                      | Auth | Description                          |
|--------|---------------------------|------|--------------------------------------|
| GET    | `/api/health`             | No   | Liveness probe                       |
| GET    | `/api/info`               | Yes  | System info (IPs, CPU, mem, disk)    |
| POST   | `/api/exec`               | Yes  | One-shot command execution           |
| POST   | `/api/exec/batch`         | Yes  | Batch command execution              |
| GET    | `/api/files`              | Yes  | Read file or list directory          |
| PUT    | `/api/files`              | Yes  | Write file (atomic)                  |
| DELETE | `/api/files`              | Yes  | Delete a file                        |
| GET    | `/api/activity`           | Yes  | Activity journal with filtering      |
| GET    | `/api/activity/{id}/result` | Yes | Cached exec result by activity ID    |
| GET    | `/api/sessions`           | Yes  | List sessions (REST)                 |
| DELETE | `/api/sessions/{id}`      | Yes  | Kill a session (REST)                |
| PATCH  | `/api/sessions/{id}`      | Yes  | Rename / AI toggle (REST)            |
| POST   | `/api/sessions/{id}/signal` | Yes | Signal a session (REST)              |
| GET    | `/api/shells`             | Yes  | List available shells                |
| GET    | `/api/gps`                | Yes  | GPS location data                    |
| GET    | `/api/playbooks`          | Yes  | List playbooks                       |
| GET    | `/api/playbooks/{name}`   | Yes  | Get playbook detail                  |
| PUT    | `/api/playbooks/{name}`   | Yes  | Create or update playbook            |
| DELETE | `/api/playbooks/{name}`   | Yes  | Delete playbook                      |
| GET    | `/api/ws`                 | Yes* | WebSocket interactive sessions       |

*WebSocket auth uses `?token=<key>` query parameter.

#### Tunnel endpoints (when `tunnel.relay = true`)

| Method | Path                                | Auth         | Description                   |
|--------|-------------------------------------|--------------|-------------------------------|
| GET    | `/api/tunnel/register`              | `tunnel_key` | Device WS registration        |
| GET    | `/api/tunnel/devices`               | `tunnel_key` | List connected devices        |
| GET    | `/d/{serial}/api/health`            | No           | Proxied device health         |
| GET    | `/d/{serial}/api/info`              | `api_key`    | Proxied device info           |
| POST   | `/d/{serial}/api/exec`              | `api_key`    | Proxied command execution     |
| POST   | `/d/{serial}/api/exec/batch`        | `api_key`    | Proxied batch execution       |
| GET    | `/d/{serial}/api/files`             | `api_key`    | Proxied file read/list        |
| PUT    | `/d/{serial}/api/files`             | `api_key`    | Proxied file write            |
| DELETE | `/d/{serial}/api/files`             | `api_key`    | Proxied file delete           |
| GET    | `/d/{serial}/api/activity`          | `api_key`    | Proxied activity journal      |
| GET    | `/d/{serial}/api/activity/{id}/result` | `api_key` | Proxied exec result           |
| GET    | `/d/{serial}/api/sessions`          | `api_key`    | Proxied session list          |
| DELETE | `/d/{serial}/api/sessions/{id}`     | `api_key`    | Proxied session kill          |
| PATCH  | `/d/{serial}/api/sessions/{id}`     | `api_key`    | Proxied session patch         |
| POST   | `/d/{serial}/api/sessions/{id}/signal` | `api_key` | Proxied session signal        |
| GET    | `/d/{serial}/api/shells`            | `api_key`    | Proxied shell list            |
| GET    | `/d/{serial}/api/playbooks`         | `api_key`    | Proxied playbook list         |
| GET    | `/d/{serial}/api/playbooks/{name}`  | `api_key`    | Proxied playbook get          |
| PUT    | `/d/{serial}/api/playbooks/{name}`  | `api_key`    | Proxied playbook put          |
| DELETE | `/d/{serial}/api/playbooks/{name}`  | `api_key`    | Proxied playbook delete       |
| GET    | `/d/{serial}/api/gps`               | `api_key`    | Proxied GPS data              |
| GET    | `/d/{serial}/api/ws`                | `api_key`    | Proxied WS sessions           |

Clients connect to the relay using the same API -- just a different base URL (`https://relay.example.com/d/DEVICE-SERIAL` instead of `http://device:1337`).

### Error codes

| HTTP | Code               | Meaning                          |
|------|--------------------|----------------------------------|
| 400  | `INVALID_REQUEST`  | Empty batch commands array       |
| 400  | `BATCH_TOO_LARGE`  | Exceeds `max_batch_size`         |
| 400  | `INVALID_PATH`     | Relative path, `..`, null bytes  |
| 400  | `IS_DIRECTORY`     | Path is directory without `list` |
| 400  | `FILE_TOO_LARGE`   | Exceeds `max_file_size`          |
| 400  | `INVALID_CONTENT`  | base64 decode failure            |
| 400  | `INVALID_MODE`     | Bad octal mode string            |
| 401  | --                 | Missing Authorization header     |
| 403  | --                 | Invalid API key                  |
| 403  | `PERMISSION_DENIED`| OS permission error              |
| 404  | `FILE_NOT_FOUND`   | File or directory missing        |
| 500  | `EXEC_FAILED`      | Spawn or wait failure            |
| 500  | `IO_ERROR`         | Filesystem I/O error             |
| 504  | `TIMEOUT`          | Command exceeded timeout         |

### GET /api/health

No authentication required.

```bash
curl http://localhost:1337/api/health
```

```json
{
  "status": "ok",
  "uptime_secs": 42,
  "version": "0.4.0",
  "sessions": 3,
  "tunnel": {
    "connected": true,
    "reconnects": 2,
    "uptime_secs": 3600,
    "messages_sent": 1234,
    "messages_received": 1230,
    "last_pong_age_ms": 5200,
    "dropped_outbound": 0,
    "rtt_median_ms": 45,
    "rtt_p95_ms": 120,
    "recent_events": [
      {"time": "5s ago", "event": "pong", "detail": "rtt=42ms"},
      {"time": "2m ago", "event": "connected", "detail": "attempt 3"}
    ]
  },
  "gps": {
    "status": "active",
    "has_fix": true,
    "fix_age_secs": 12,
    "satellites": 8
  }
}
```

The `tunnel` object is included when tunnel client mode is configured. Full metrics (uptime, messages, RTT, events) appear for client mode; relay mode shows only `connected` and `reconnects`. The `gps` object is included when `[gps]` is configured (null otherwise).

### GET /api/info

Returns system information: hostname, kernel, CPU, memory, disk, and network interfaces. Conditionally includes `tunnel`, `gps`, and `lte` sections when configured.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/info
```

```json
{
  "serial": "SCTL-0001-DEV-001",
  "hostname": "router-1",
  "kernel": "Linux 5.4.260",
  "system_uptime_secs": 86400,
  "cpu_model": "ARMv7 Processor rev 3 (v7l)",
  "load_average": [0.12, 0.08, 0.05],
  "memory": {"total_bytes": 2097152000, "available_bytes": 1572864000, "used_bytes": 524288000},
  "disk": {"path": "/", "total_bytes": 8000000000, "used_bytes": 2000000000, "available_bytes": 6000000000},
  "interfaces": [
    {"name": "eth0", "state": "UP", "mac": "02:00:00:00:00:01", "addresses": ["192.168.1.1/24"]}
  ],
  "tunnel": {
    "connected": true,
    "relay_url": "wss://relay.example.com/api/tunnel/register",
    "reconnects": 2
  },
  "gps": {
    "status": "active",
    "latitude": 45.5017,
    "longitude": -73.5673,
    "altitude": 50.2,
    "satellites": 8,
    "speed_kmh": 0.0,
    "hdop": 1.2,
    "fix_age_secs": 12
  },
  "lte": {
    "rssi_dbm": -75,
    "rsrp": -105,
    "rsrq": -12,
    "sinr": 8.5,
    "band": "B4",
    "operator": "Rogers",
    "technology": "LTE",
    "cell_id": "1A2B3C4",
    "signal_bars": 3,
    "modem": {
      "model": "EC25",
      "firmware": "EC25AFAR06A06M4G",
      "imei": "860000000000000",
      "iccid": "89000000000000000000"
    }
  }
}
```

The `tunnel`, `gps`, and `lte` sections are only present when the corresponding feature is configured.

### POST /api/exec

Execute a single command.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/exec \
  -H "Content-Type: application/json" \
  -d '{"command": "df -h /", "timeout_ms": 5000, "request_id": "req-1"}'
```

```json
{
  "exit_code": 0,
  "stdout": "Filesystem      Size  Used Avail Use% Mounted on\n/dev/root       7.4G  2.1G  5.0G  30% /\n",
  "stderr": "",
  "duration_ms": 12,
  "request_id": "req-1"
}
```

Request fields:

| Field         | Type   | Required | Description                            |
|---------------|--------|----------|----------------------------------------|
| `command`     | string | yes      | Shell command (`<shell> -c "<cmd>"`)   |
| `timeout_ms`  | number | no       | Override default timeout               |
| `request_id`  | string | no       | Echoed in response for correlation     |
| `working_dir` | string | no       | Override working directory              |
| `env`         | object | no       | Extra env vars (merged, not replacing) |
| `shell`       | string | no       | Override shell binary                  |

> **Note:** `stdout` and `stderr` are each capped at 1 MB. If output exceeds the limit, it is truncated and `"[truncated at 1048576 bytes]"` is appended.

### POST /api/exec/batch

Execute multiple commands sequentially. A failing command does not abort the batch.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/exec/batch \
  -H "Content-Type: application/json" \
  -d '{
    "commands": [
      {"command": "uptime"},
      {"command": "free -m"}
    ],
    "request_id": "batch-1"
  }'
```

Top-level `shell`, `working_dir`, and `env` apply as defaults. Per-command fields override them (env is merged, command-level wins).

Response:

```json
{
  "results": [
    {"exit_code": 0, "stdout": " 12:34:56 up 1 day, ...\n", "stderr": "", "duration_ms": 8},
    {"exit_code": 0, "stdout": "       total    used    free ...\n", "stderr": "", "duration_ms": 5}
  ],
  "request_id": "batch-1"
}
```

### GET /api/files

Read a file or list a directory.

```bash
# Read a file
curl -H "Authorization: Bearer $KEY" "http://localhost:1337/api/files?path=/etc/hostname"

# List a directory
curl -H "Authorization: Bearer $KEY" "http://localhost:1337/api/files?path=/etc/&list=true"
```

File read response:

```json
{"path": "/etc/hostname", "content": "router-1\n", "encoding": "utf8", "size": 9}
```

Directory listing response:

```json
{
  "path": "/etc/config",
  "entries": [
    {"name": "network", "type": "file", "size": 1234},
    {"name": "wireless", "type": "file", "size": 567},
    {"name": "logs", "type": "symlink", "target": "/var/log"}
  ]
}
```

Files are returned as UTF-8 text, or base64 with `"encoding": "base64"` for binary content. Symlinks are detected with their targets resolved.

### PUT /api/files

Write a file atomically (temp-then-rename).

```bash
curl -X PUT -H "Authorization: Bearer $KEY" http://localhost:1337/api/files \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/test.txt",
    "content": "Hello, world!\n",
    "mode": "0644",
    "create_dirs": true
  }'
```

| Field         | Type   | Required | Description                              |
|---------------|--------|----------|------------------------------------------|
| `path`        | string | yes      | Absolute destination path                |
| `content`     | string | yes      | File contents (UTF-8 or base64)          |
| `encoding`    | string | no       | Set to `"base64"` for binary content     |
| `mode`        | string | no       | Octal permissions (e.g. `"0644"`)        |
| `create_dirs` | bool   | no       | Create parent directories if missing     |

### DELETE /api/files

Delete a file.

```bash
curl -X DELETE -H "Authorization: Bearer $KEY" http://localhost:1337/api/files \
  -H "Content-Type: application/json" \
  -d '{"path": "/tmp/test.txt"}'
```

| Field  | Type   | Required | Description                |
|--------|--------|----------|----------------------------|
| `path` | string | yes      | Absolute path to delete    |

Returns `200` with `{"deleted": "/tmp/test.txt"}` on success. Returns `404 FILE_NOT_FOUND` if the file does not exist, `403 PERMISSION_DENIED` on OS permission errors.

### GET /api/activity

Read recent activity entries with optional filtering.

```bash
curl -H "Authorization: Bearer $KEY" \
  "http://localhost:1337/api/activity?since_id=0&limit=50&activity_type=exec&source=mcp"
```

Query parameters:

| Parameter       | Type   | Default | Description                              |
|-----------------|--------|---------|------------------------------------------|
| `since_id`      | number | `0`     | Return entries with `id > since_id`      |
| `limit`         | number | `50`    | Maximum entries to return (max 200)      |
| `activity_type` | string | --      | Filter by type (e.g. `exec`, `file_read`, `session_start`) |
| `source`        | string | --      | Filter by source (e.g. `mcp`, `ws`, `rest`) |
| `session_id`    | string | --      | Filter by session ID                     |

```json
{
  "entries": [
    {
      "id": 42,
      "activity_type": "exec",
      "source": "rest",
      "summary": "uname -a",
      "detail": {"exit_code": 0, "duration_ms": 12},
      "timestamp": "2026-02-26T12:00:00Z"
    }
  ]
}
```

### GET /api/activity/{id}/result

Retrieve a cached full exec result by activity ID.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/activity/42/result
```

Returns the full stdout/stderr/exit-code for the given activity ID. Returns `404 NOT_FOUND` if the result has been evicted from the in-memory cache (max `exec_result_cache_size` entries, default 100).

### GET /api/gps

Returns GPS status, last fix, and fix history. Returns `404` if GPS is not configured.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/gps
```

```json
{
  "status": "active",
  "last_fix": {
    "latitude": 45.5017,
    "longitude": -73.5673,
    "altitude": 50.2,
    "speed_kmh": 0.0,
    "course": 180.0,
    "hdop": 1.2,
    "satellites": 8,
    "utc": "120000.00",
    "date": "260226",
    "fix_type": "3D",
    "recorded_at": "2026-02-26T12:00:00Z"
  },
  "fix_age_secs": 12,
  "history": [
    {
      "latitude": 45.5017,
      "longitude": -73.5673,
      "altitude": 50.2,
      "speed_kmh": 0.0,
      "satellites": 8,
      "recorded_at": "2026-02-26T12:00:00Z"
    }
  ],
  "fixes_total": 1234,
  "errors_total": 5,
  "last_error": "timeout waiting for AT response"
}
```

### GET /api/playbooks

List all playbooks with name, description, and parameters.

```bash
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/playbooks
```

### GET/PUT/DELETE /api/playbooks/{name}

- **GET** — returns playbook detail (metadata, params, script, raw content)
- **PUT** — create or update a playbook (request body is raw markdown)
- **DELETE** — delete a playbook

```bash
# Get a playbook
curl -H "Authorization: Bearer $KEY" http://localhost:1337/api/playbooks/health-check

# Create/update a playbook
curl -X PUT -H "Authorization: Bearer $KEY" \
  -H "Content-Type: text/markdown" \
  --data-binary @playbook.md \
  http://localhost:1337/api/playbooks/health-check

# Delete a playbook
curl -X DELETE -H "Authorization: Bearer $KEY" \
  http://localhost:1337/api/playbooks/health-check
```

## WebSocket Protocol

Connect to `GET /api/ws?token=<api_key>`. All messages are JSON with a `"type"` field. An optional `"request_id"` on any client message is echoed back.

### Client messages

| Type                | Fields                                                                            | Response                             |
|---------------------|-----------------------------------------------------------------------------------|--------------------------------------|
| `ping`              | --                                                                                | `pong`                               |
| `session.start`     | `working_dir?`, `persistent?`, `env?`, `shell?`, `pty?`, `rows?`, `cols?`, `idle_timeout?`, `name?` | `session.started` or `error` |
| `session.exec`      | `session_id`, `command`                                                           | `session.exec.ack` or `error`        |
| `session.stdin`     | `session_id`, `data`                                                              | (none on success)                    |
| `session.kill`      | `session_id`                                                                      | `session.closed` or `error`          |
| `session.signal`    | `session_id`, `signal`                                                            | `session.signal.ack` or `error`      |
| `session.attach`    | `session_id`, `since?`                                                            | `session.attached` or `error`        |
| `session.list`      | --                                                                                | `session.listed`                     |
| `session.resize`    | `session_id`, `rows`, `cols`                                                      | `session.resize.ack` or `error`      |
| `session.rename`    | `session_id`, `name`                                                              | `session.rename.ack` or `error`      |
| `session.allow_ai`  | `session_id`, `allowed`                                                           | `session.allow_ai.ack` or `error`    |
| `session.ai_status` | `session_id`, `working`, `activity?`, `message?`                                  | `session.ai_status.ack` or `error`   |
| `shell.list`        | --                                                                                | `shell.listed`                       |

### Server messages

| Type                            | Key fields                                                                |
|---------------------------------|---------------------------------------------------------------------------|
| `pong`                          | --                                                                        |
| `session.started`               | `session_id`, `pid`, `persistent`, `pty`                                  |
| `session.exec.ack`              | `session_id`                                                              |
| `session.stdout`                | `session_id`, `data`, `seq`, `timestamp_ms`                               |
| `session.stderr`                | `session_id`, `data`, `seq`, `timestamp_ms`                               |
| `session.system`                | `session_id`, `data`, `seq`, `timestamp_ms`                               |
| `session.exited`                | `session_id`, `exit_code`                                                 |
| `session.closed`                | `session_id`, `reason`                                                    |
| `session.signal.ack`            | `session_id`, `signal`                                                    |
| `session.attached`              | `session_id`, `entries[]`, `dropped`                                      |
| `session.listed`                | `sessions[]` (id, pid, persistent, pty, attached, status, idle, name ...) |
| `session.resize.ack`            | `session_id`                                                              |
| `session.rename.ack`            | `session_id`, `name`                                                      |
| `session.allow_ai.ack`          | `session_id`, `allowed`                                                   |
| `session.ai_status.ack`         | `session_id`, `working`                                                   |
| `session.renamed`               | `session_id`, `name` (broadcast)                                          |
| `session.ai_permission_changed` | `session_id`, `allowed` (broadcast)                                       |
| `session.ai_status_changed`     | `session_id`, `working`, `activity`, `message` (broadcast)                |
| `shell.listed`                  | `shells[]`, `default`                                                     |
| `gps.fix`                       | `latitude`, `longitude`, `altitude`, `satellites`, `speed_kmh` (broadcast)|
| `lte.signal`                    | `rssi_dbm`, `signal_bars`, `band`, `operator`, `technology` (broadcast)   |
| `activity.new`                  | `entry` (broadcast on every new activity log entry)                       |
| `error`                         | `code`, `message`, `session_id?`                                          |

Output messages (`session.stdout`, `session.stderr`, `session.system`) include a monotonically increasing `seq` number and a `timestamp_ms` field. Clients use `seq` for reliable catch-up via `session.attach`.

### session.start fields

| Field          | Type   | Default                   | Description                                                |
|----------------|--------|---------------------------|------------------------------------------------------------|
| `working_dir`  | string | server `default_working_dir` | Initial working directory                               |
| `persistent`   | bool   | `false`                   | Survive WS disconnects (detach instead of kill)            |
| `env`          | object | --                        | Extra environment variables                                |
| `shell`        | string | server `default_shell`    | Shell binary path                                          |
| `pty`          | bool   | `false`                   | Allocate PTY for full terminal emulation                   |
| `rows`         | number | `24`                      | Initial PTY rows (only with `pty: true`)                   |
| `cols`         | number | `80`                      | Initial PTY columns (only with `pty: true`)                |
| `idle_timeout` | number | `0`                       | Seconds of inactivity (while detached) before auto-kill. 0 = never. |
| `name`         | string | --                        | Human-readable session name                                |

### Persistent sessions

By default, sessions are killed when the WebSocket disconnects. Set `persistent: true` on `session.start` to keep sessions alive across disconnects:

- **Non-persistent** (default): Killed on WS disconnect.
- **Persistent**: Detached on WS disconnect. Output keeps buffering (up to `session_buffer_size` entries). Re-attach later with `session.attach` to catch up on missed output.

Detached persistent sessions with a non-zero `idle_timeout` are automatically cleaned up by a sweep task that runs every 30 seconds. Sessions with `idle_timeout: 0` remain alive until explicitly killed or the server restarts.

### PTY sessions

Set `pty: true` on `session.start` to allocate a pseudo-terminal. PTY sessions support:

- ANSI escape codes, colors, cursor movement
- Interactive programs (vim, htop, top, etc.)
- Terminal resize via `session.resize`
- Login shell (`-l` flag) with proper terminal environment

Without `pty: true`, sessions use pipe-based I/O (suitable for scripted commands but no terminal emulation).

### Process groups and signals

Sessions are spawned in their own process group (`setpgid(0, 0)`). The `session.signal` message sends a signal to the entire process group, giving real Ctrl-C behavior:

```
->  {"type": "session.signal", "session_id": "abc-123", "signal": 2}
<-  {"type": "session.signal.ack", "session_id": "abc-123", "signal": 2}
```

Common signals: `2` = SIGINT (Ctrl-C), `15` = SIGTERM, `9` = SIGKILL.

### AI collaboration

Sessions support AI/human collaboration via permission and status tracking:

- **`session.allow_ai`** -- toggle whether AI is allowed to send input to a session
- **`session.ai_status`** -- AI reports its working state (`working`, `activity`, `message`)
- Changes are broadcast to all connected clients for real-time UI updates

## Reverse Tunnel

sctl includes a built-in reverse tunnel for devices behind CGNAT (LTE/5G connections) that can't accept inbound connections. Any sctl instance can act as a **relay** -- devices connect outbound and clients reach them through it.

```
Device (behind CGNAT)                 Relay (VPS)                      Clients
 +--------+                           +-------------+                  +---------+
 | sctl   |--- outbound WS ---------> | sctl        | <--- HTTP/WS -- | mcp-sctl|
 | server |   (registers serial)      | (relay mode)|   (same API)    | sctlin  |
 +--------+                           +-------------+                  +---------+
```

**Relay mode** -- run on a VPS or any publicly reachable host:

```toml
[tunnel]
relay = true
tunnel_key = "shared-secret"
```

**Client mode** -- run on a CGNAT device:

```toml
[tunnel]
tunnel_key = "shared-secret"
url = "wss://relay.example.com/api/tunnel/register"
```

Clients just use a different base URL. No changes to mcp-sctl or sctlin:

- Direct: `http://10.42.0.192:1337`
- Via relay: `https://relay.example.com/d/BPI-RV2-V11-001`

Devices register dynamically with their serial and API key. The relay learns devices on connect and routes client requests through the tunnel. Sessions, exec, files, and info all work transparently.

### Example session

```
->  {"type": "session.start", "persistent": true, "pty": true, "name": "debug"}
<-  {"type": "session.started", "session_id": "abc-123", "pid": 4567, "persistent": true, "pty": true}

->  {"type": "session.exec", "session_id": "abc-123", "command": "ls /"}
<-  {"type": "session.exec.ack", "session_id": "abc-123"}
<-  {"type": "session.stdout", "session_id": "abc-123", "data": "bin\ndev\netc\n...", "seq": 0}

->  {"type": "session.kill", "session_id": "abc-123"}
<-  {"type": "session.closed", "session_id": "abc-123", "reason": "killed"}
```

### Reconnection

After a disconnect, persistent sessions can be re-attached:

```
->  {"type": "session.attach", "session_id": "abc-123", "since": 42}
<-  {"type": "session.attached", "session_id": "abc-123", "entries": [...], "dropped": 0}
```

The `since` field is the last `seq` the client received. The server replays all buffered entries after that point. If entries were evicted from the ring buffer, `dropped` indicates how many were lost.

## Deployment

### Cross-compilation

sctl uses [`cross`](https://github.com/cross-rs/cross) for static musl builds targeting embedded devices:

```bash
# Install cross (Docker-based cross-compiler)
cargo install cross --git https://github.com/cross-rs/cross

# ARM (e.g. Raspberry Pi, OpenWrt ARM routers)
cross build --release --target armv7-unknown-linux-musleabihf

# RISC-V (e.g. BPI-RV2, OpenWrt RISC-V routers)
cross build --release --target riscv64gc-unknown-linux-musl
```

Or use the Makefile:

```bash
make build-arm     # ARM build
make build-riscv   # RISC-V build
```

### Deploy to device

The easiest way to deploy is with `rundev.sh`, which auto-detects architecture and handles cross-compilation:

```bash
# One-time: discover device (probes arch, serial, api_key via SSH)
./rundev.sh device add mydevice 192.168.1.1

# Full deploy: cross-compile + binary + config + init script
./rundev.sh device deploy mydevice

# Later upgrades: binary-only (stop → upload → start)
./rundev.sh device upgrade mydevice
```

Alternatively, use the Makefile directly:

```bash
make deploy HOST=192.168.1.1         # ARM
make deploy-riscv HOST=192.168.1.1   # RISC-V
```

Both approaches copy the binary, config, and init script, then enable the service.

For OpenWrt devices, a procd init script is included at `files/sctl.init`.

## Security

### Authentication

All endpoints except `/api/health` require authentication:

- **HTTP**: `Authorization: Bearer <key>` header
- **WebSocket**: `?token=<key>` query parameter (browsers can't set headers during WS upgrade)

The API key is compared using constant-time comparison (`auth.rs`) to prevent timing side-channel attacks.

### Path validation

File operations reject paths containing `..` components, null bytes, or relative paths -- preventing path traversal attacks.

### Process management

- All child processes use `kill_on_drop(true)` -- no orphaned processes if the server crashes or a task is cancelled
- Sessions are spawned in their own process group (`setpgid(0, 0)`) -- signals reach the entire process tree
- Session creation holds a write lock across the limit check and insert to prevent TOCTOU races
- Output capture reads stdout and stderr concurrently to prevent pipe deadlocks
- Output past the 1 MB cap is drained (not pipe-closed) to avoid SIGPIPE
- Session output is buffer-backed (not WS-coupled) -- sessions survive connection drops

### Atomic file writes

File writes use a temp-file-then-rename pattern, so readers never see partial content.

## Development

### Prerequisites

- Rust 1.82+
- Docker (for `cross` ARM/RISC-V builds)

### Makefile targets

```bash
make dev          # Run locally with debug logging
make build        # Build release binary
make build-arm    # Cross-compile for ARM
make build-riscv  # Cross-compile for RISC-V
make deploy HOST= # Deploy to device (ARM)
make deploy-riscv HOST= # Deploy to device (RISC-V)
make fmt          # Check formatting
make lint         # Run clippy lints
make test         # Run tests
make doc          # Generate docs
make doc-open     # Generate and open docs in browser
make check        # Run all quality checks (fmt + lint + test + build)
```

### Running locally

```bash
SCTL_API_KEY=dev-key RUST_LOG=debug cargo run
```

## License

GPL-3.0-only. See [LICENSE](../LICENSE).
