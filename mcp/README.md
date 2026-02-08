<p align="center">
  <img src="../sctl-logo.png" alt="sctl" width="120" />
</p>

# mcp-sctl

MCP proxy that gives AI agents (Claude, GPT, local models) hands-on access to remote Linux devices via [sctl](../README.md).

## Overview

mcp-sctl runs as a stdio-based MCP server, translating JSON-RPC tool calls into sctl HTTP and WebSocket requests. It supports multiple named devices, interactive streaming sessions with auto-reconnect, and local output buffering for zero-latency reads.

```
                 stdio (JSON-RPC)              HTTP + WebSocket
┌──────────────┐ <---------------> ┌──────────────────┐ <---------------> ┌─────────────┐
│  AI Agent    │                   │    mcp-sctl      │                   │    sctl     │
│  (Claude,    │   MCP protocol    │                  │   REST + WS       │  (device)   │
│   etc.)      │                   │  Local buffers   │   streaming       │             │
└──────────────┘                   │  Auto-reconnect  │                   └─────────────┘
                                   └──────────────────┘
```

**Device tools** use the REST API for one-shot operations (exec, file read/write, health checks). **Session tools** use WebSocket for persistent interactive shells with real-time output streaming.

## Quick Start

### Single device (environment variables)

```bash
export SCTL_URL=http://192.168.1.1:1337
export SCTL_API_KEY=your-secret-key

cargo run
```

### Multiple devices (config file)

```bash
cargo run -- --config devices.json
```

See [devices.example.json](devices.example.json) for the config format:

```json
{
  "devices": {
    "router-1": {
      "url": "http://192.168.1.1:1337",
      "api_key": "key-for-router-1",
      "playbooks_dir": "/etc/sctl/playbooks"
    },
    "router-2": {
      "url": "http://192.168.1.2:1337",
      "api_key": "key-for-router-2"
    }
  },
  "default_device": "router-1"
}
```

### Claude Code integration

Register directly:

```bash
claude mcp add sctl -- /path/to/mcp-sctl --config /path/to/devices.json
```

Or add to `~/.claude/claude_code_config.json`:

```json
{
  "mcpServers": {
    "sctl": {
      "command": "/path/to/mcp-sctl",
      "args": ["--config", "/path/to/devices.json"]
    }
  }
}
```

## Configuration

Configuration is resolved from three sources (tried in order):

1. **`--config <path>`** CLI flag -- JSON file with multiple named devices
2. **`SCTL_CONFIG`** env var -- path to the same JSON file format
3. **`SCTL_URL` + `SCTL_API_KEY`** env vars -- creates a single "default" device

### Config file format

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `devices` | object | yes | Map of device name to `{url, api_key, playbooks_dir?}` |
| `default_device` | string | no | Default device name. Required if multiple devices. Auto-detected if only one device. |

Per-device fields:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | yes | sctl server URL (e.g. `http://192.168.1.1:1337`) |
| `api_key` | string | yes | Bearer token for authentication |
| `playbooks_dir` | string | no | Path to playbooks directory on device (default `/etc/sctl/playbooks`) |

## Tools

### Device Tools (HTTP)

One-shot HTTP requests -- no persistent connection needed.

#### `device_list`

List all configured devices and the default device name.

#### `device_health`

Check if a device is alive. Returns uptime and version.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `device` | string | no | Device name (defaults to default device) |

#### `device_info`

Get system information: hostname, IPs, CPU, memory, disk, network interfaces.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `device` | string | no | Device name |

#### `device_exec`

Execute a shell command and return stdout, stderr, and exit code.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Shell command to execute |
| `device` | string | no | Device name |
| `timeout_ms` | integer | no | Timeout in ms (default 30000) |
| `working_dir` | string | no | Working directory (absolute path) |
| `env` | object | no | Environment variables |

#### `device_exec_batch`

Execute multiple commands sequentially. Each item can be a string or an object with `command`, `timeout_ms`, `working_dir`, `env` fields.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `commands` | array | yes | Commands to execute |
| `device` | string | no | Device name |
| `working_dir` | string | no | Default working directory |
| `env` | object | no | Default environment variables |

#### `device_file_read`

Read a file or list a directory.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path |
| `device` | string | no | Device name |
| `list` | boolean | no | List directory entries (default false) |

#### `device_file_write`

Write content to a file atomically (temp file + rename).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path |
| `content` | string | yes | File content (UTF-8 or base64) |
| `device` | string | no | Device name |
| `encoding` | string | no | `"base64"` for binary content |
| `mode` | string | no | File permissions (e.g. `"0644"`) |
| `create_dirs` | boolean | no | Create parent directories (default false) |

### Session Tools (WebSocket)

Session tools provide persistent interactive shells. Output is buffered both server-side (in sctl's `OutputBuffer`) and client-side (in mcp-sctl's local `SessionBuffer`), so `session_read` returns instantly from local memory.

Sessions survive WebSocket disconnects -- mcp-sctl automatically reconnects and re-attaches with the last known sequence number, replaying any missed output.

#### `session_start`

Start a new interactive shell session. Returns a `session_id` for subsequent calls.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `device` | string | no | Device name |
| `working_dir` | string | no | Initial working directory |
| `shell` | string | no | Shell binary (e.g. `/bin/bash`) |
| `env` | object | no | Environment variables |
| `persistent` | boolean | no | Survive WS disconnects (default true) |
| `pty` | boolean | no | Allocate PTY for full terminal emulation (default false) |
| `rows` | integer | no | PTY rows (default 24, only with `pty: true`) |
| `cols` | integer | no | PTY columns (default 80, only with `pty: true`) |
| `idle_timeout` | integer | no | Seconds of inactivity (while detached) before auto-kill. 0 = never (default). |
| `name` | string | no | Human-readable session name |

Returns: `{session_id, pid, persistent, pty}`

#### `session_exec`

Execute a command in an existing session (appends newline). Use `session_read` to get output.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID from `session_start` |
| `command` | string | yes | Command to execute |
| `device` | string | no | Device name |

#### `session_send`

Send raw data to a session's stdin (no newline appended). Useful for interactive prompts, passwords, or special key sequences.

Unicode escape sequences (e.g. `\u0003`) are automatically converted to their byte values. For PTY sessions, `\n` is automatically translated to `\r` (carriage return).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `data` | string | yes | Raw data to send |
| `device` | string | no | Device name |

#### `session_read`

Read buffered output from a session. Returns entries since the given sequence number. Waits up to `timeout_ms` for new output if none is available.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `since` | integer | no | Sequence number (default 0 = from beginning) |
| `timeout_ms` | integer | no | Wait timeout in ms (default 5000) |
| `device` | string | no | Device name |

Returns: `{entries: [{seq, stream, data, timestamp_ms}], last_seq, status, exit_code, dropped_entries}`

- `stream`: `"stdout"`, `"stderr"`, or `"system"`
- `status`: `"running"` or `"exited"`
- `dropped_entries`: number of entries lost due to buffer overflow
- Pass `last_seq` as `since` on the next call to get only new output

#### `session_signal`

Send a POSIX signal to the session's process group.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `signal` | integer | yes | Signal number (2=SIGINT, 15=SIGTERM, 9=SIGKILL) |
| `device` | string | no | Device name |

#### `session_kill`

Kill a session and its entire process group. The session is permanently destroyed.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `device` | string | no | Device name |

#### `session_list`

List all active sessions on a device (including sessions created by other clients).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `device` | string | no | Device name (omit to list across all devices) |

#### `session_attach`

Re-attach to an existing persistent session after a disconnect.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `since` | integer | no | Sequence number (0 = replay from beginning) |
| `device` | string | no | Device name |

#### `session_exec_wait`

Execute a command and wait for completion. Returns full output and exit code in a single call. Uses marker-based completion detection.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `command` | string | yes | Command to execute |
| `timeout_ms` | integer | no | Max wait time (default 30000) |
| `device` | string | no | Device name |

Returns: `{output, exit_code, timed_out}`

#### `session_resize`

Resize the terminal for a PTY session.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `rows` | integer | yes | Terminal rows |
| `cols` | integer | yes | Terminal columns |
| `device` | string | no | Device name |

#### `session_rename`

Rename a session. The new name is broadcast to all connected clients.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `name` | string | yes | New session name |
| `device` | string | no | Device name |

#### `session_allow_ai`

Toggle whether AI is allowed to send input to a session. Used for AI/human handoff -- when AI is disallowed, only the web UI can control the session.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `allowed` | boolean | yes | Whether AI input is allowed |
| `device` | string | no | Device name |

#### `session_ai_status`

Report AI working status for a session. The status is broadcast to all connected clients for real-time UI feedback.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Session ID |
| `working` | boolean | yes | Whether AI is actively working |
| `activity` | string | no | Activity type: `"read"` or `"write"` |
| `message` | string | no | Human-readable status message |
| `device` | string | no | Device name |

### Playbook Tools

Playbooks are markdown files with YAML frontmatter stored on devices. They are automatically discovered and exposed as MCP tools with the `pb_` prefix.

#### `playbook_list`

List playbooks from one or all devices.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `device` | string | no | Device name (omit to list from all) |

#### `playbook_get`

Get the full markdown content of a playbook.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Playbook name (without `.md`) |
| `device` | string | no | Device name |

#### `playbook_put`

Create, update, or delete a playbook. Empty content deletes.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Playbook name (without `.md`) |
| `content` | string | yes | Full markdown content (empty = delete) |
| `device` | string | no | Device name |

## Architecture

```
src/
├── main.rs              # Entry point: parse CLI, load config, run MCP loop
├── mcp.rs               # JSON-RPC 2.0 handler (initialize, tools/list, tools/call)
├── config.rs            # Configuration loading (CLI, env vars, JSON file)
├── devices.rs           # Device registry, WsPool for lazy WS connections
├── client.rs            # HTTP client for sctl REST endpoints
├── tools.rs             # Tool definitions (JSON schemas) and handlers
├── websocket.rs         # WS client, auto-reconnect, local session buffers
├── playbooks.rs         # Playbook model, YAML frontmatter parsing
└── playbook_registry.rs # Per-device playbook cache with lazy fetch
```

### Data flow

**Device tools** (one-shot): `AI -> MCP stdin -> tools.rs -> client.rs -> HTTP -> sctl -> response -> MCP stdout -> AI`

**Session tools** (streaming): `AI -> MCP stdin -> tools.rs -> websocket.rs -> WS -> sctl -> WS stream -> local buffer -> session_read -> MCP stdout -> AI`

## Development

```bash
# Build
make build

# Run locally (single device)
make dev

# Check formatting + lints + build
make check

# Generate docs
make doc
```

### Prerequisites

- Rust 1.75+
- A running sctl instance to connect to

### Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (stdio, timers, sync primitives) |
| `serde` / `serde_json` | JSON serialization |
| `reqwest` | HTTP client for REST endpoints |
| `tokio-tungstenite` | WebSocket client for session streaming |
| `futures-util` | Stream/Sink utilities for WS I/O |
| `clap` | CLI argument parsing |
| `serde_yaml` | Playbook frontmatter parsing |

## License

GPL-3.0-only. See [LICENSE](../LICENSE).
