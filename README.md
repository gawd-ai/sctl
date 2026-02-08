<p align="center">
  <img src="sctl-logo.png" alt="sctl" width="160" />
</p>

<h1 align="center">sctl</h1>

<p align="center">
  <strong>Shell Control — pronounced "scuttle" (yes, the Rustacean pun is intended).</strong><br/>
  Give AI agents hands-on access to Linux devices.<br/>
  Execute commands, manage interactive shell sessions, and read/write files — via <a href="https://modelcontextprotocol.io/">MCP</a> or direct HTTP/WebSocket API.
</p>

<p align="center">
  <a href="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml"><img src="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue.svg" alt="License: GPL-3.0" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.75%2B-orange.svg" alt="Rust 1.75+" /></a>
</p>

---

> **WARNING: SCTL IS AN EXTREMELY POWERFUL TOOL AND WITH GREAT POWER COMES GREAT RESPONSIBILITY. THIS TOOL CAN BE DANGEROUS AND CAUSE DESTRUCTION AND TOTAL OBLITERATION IF NOT USED WITH ABSOLUTE DILIGENCE. THE AUTHORS AND GAWD BEAR NO RESPONSIBILITY FOR ANY OUTCOME WHATSOEVER.**

---

sctl is a two-component system that lets AI agents (Claude, GPT, local models) or any authenticated client remotely operate Linux devices with full terminal access:

| Component | What it does |
|-----------|-------------|
| **[sctl](server/)** | Lightweight server that runs on the target device (ARM or x86) |
| **[mcp-sctl](mcp/)** | MCP proxy that translates AI tool calls into sctl API requests |
| **[sctlin](web/)** | Svelte 5 terminal UI component library with xterm.js |

```
                                                         ┌────────────────┐
┌────────────────┐  stdio (MCP)  ┌────────────────┐      │                │
│                │ ◄───────────► │                │ HTTP │  sctl          │
│    AI Agent    │  JSON-RPC 2.0 │   mcp-sctl     │ ◄──► │  (device)      │
│                │               │  Multi-device  │ +WS+ │                │
└────────────────┘               └────────────────┘      │  Linux / ARM   │
                                                         └────────────────┘
```

## Why sctl?

Most AI agents can run commands locally. **sctl lets them operate remote devices** — routers, servers, IoT devices, VMs — with persistent sessions that survive network blips, full PTY terminal emulation, and security-first design.

- **Persistent sessions** — shells survive disconnects, output keeps buffering, re-attach and catch up
- **Full PTY support** — run vim, htop, docker, anything that needs a real terminal
- **Multi-device fleet** — manage many devices from one MCP server
- **Auto-reconnect** — WebSocket drops are handled transparently with output replay
- **Playbooks** — device-stored scripts auto-discovered as MCP tools
- **Security-first** — constant-time auth, path traversal prevention, process isolation, atomic writes

## Quick Start

### Option 1: Claude Code (recommended)

The fastest path from zero to AI-controlled devices:

```bash
git clone https://github.com/gawd-ai/sctl.git && cd sctl

# Build everything and start the dev stack
chmod +x rundev.sh
./rundev.sh
```

This builds the server + MCP proxy, starts sctl locally, and registers it with Claude Code. Open a new Claude Code conversation and your AI can now execute commands, start shell sessions, and manage files on your machine.

### Option 2: Manual Setup

**1. Start the server** on the target device:

```bash
cd server && cargo build --release

SCTL_API_KEY=your-secret-key ./target/release/sctl
# Listening on 0.0.0.0:1337
```

Verify it's running:

```bash
curl http://localhost:1337/api/health
# {"status":"ok","uptime_secs":5,"version":"0.3.0"}
```

**2. Start the MCP proxy** on your dev machine:

```bash
cd mcp && cargo build --release

# Single device via env vars
export SCTL_URL=http://your-device:1337
export SCTL_API_KEY=your-secret-key

# Register with Claude Code
claude mcp add sctl -- ./target/release/mcp-sctl
```

**3. Use it.** Open Claude Code and ask it to run commands on your device:

> "Check the disk usage and running processes on my device"

The AI will use `device_exec`, `session_start`, and other MCP tools automatically.

### Option 3: Direct HTTP API

No MCP needed — use sctl's REST API directly:

```bash
# Execute a command
curl -H "Authorization: Bearer your-secret-key" \
     -H "Content-Type: application/json" \
     -d '{"command": "uname -a"}' \
     http://localhost:1337/api/exec

# Read a file
curl -H "Authorization: Bearer your-secret-key" \
     "http://localhost:1337/api/files?path=/etc/hostname"

# Start an interactive WebSocket session
websocat "ws://localhost:1337/api/ws?token=your-secret-key"
```

## MCP Tool Reference

When connected via MCP, AI agents get these tools:

| Tool | Description |
|------|-------------|
| `device_list` | List configured devices |
| `device_health` | Check if a device is alive |
| `device_info` | System info (hostname, CPU, memory, disk, network) |
| `device_exec` | Execute a shell command |
| `device_exec_batch` | Execute multiple commands sequentially |
| `device_file_read` | Read a file or list a directory |
| `device_file_write` | Write a file atomically |
| `session_start` | Start a persistent interactive shell (with optional PTY) |
| `session_exec` | Run a command in a session |
| `session_send` | Send raw input (arrow keys, Ctrl sequences) |
| `session_read` | Read buffered output |
| `session_exec_wait` | Execute and wait for completion in one call |
| `session_signal` | Send POSIX signals (SIGINT, SIGTERM, etc.) |
| `session_kill` | Kill a session |
| `session_list` | List active sessions |
| `session_attach` | Re-attach to an existing persistent session |
| `session_resize` | Resize PTY terminal dimensions |
| `session_rename` | Rename a session |
| `session_allow_ai` | Toggle AI input permission (human/AI handoff) |
| `session_ai_status` | Report AI working status for UI feedback |
| `playbook_list` | List device-stored playbooks |
| `playbook_get` | Read a playbook |
| `playbook_put` | Create/update/delete playbooks |

## Multi-Device Configuration

Manage a fleet of devices from a single MCP server:

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
    },
    "prod-server": {
      "url": "http://10.0.0.5:1337",
      "api_key": "key-for-prod"
    }
  },
  "default_device": "router-1"
}
```

```bash
mcp-sctl --config devices.json
```

## Deployment

### Any Linux

```bash
SCTL_API_KEY=change-me ./sctl serve --config sctl.toml
```

### OpenWrt / Embedded ARM

```bash
cd server

# Cross-compile for ARMv7
make build-arm

# Deploy to device (copies binary, config, init script)
make deploy HOST=192.168.1.1
```

A [procd init script](server/files/sctl.init) is included for OpenWrt service management with auto-restart.

### Supervisor Mode

sctl includes a built-in supervisor with exponential backoff restart:

```bash
./sctl supervise --config sctl.toml
```

## Security

sctl is designed for deployment on real devices in real networks:

- **Authentication** -- pre-shared API key with constant-time comparison (timing side-channel resistant)
- **Path validation** -- rejects traversal attacks (`..`, null bytes, relative paths)
- **Process isolation** -- sessions in own process groups, `kill_on_drop` on all children
- **Atomic writes** -- temp-file-then-rename prevents partial reads
- **Resource limits** -- configurable caps on sessions, file sizes, timeouts, batch sizes

See the [security review](server/docs/REVIEW.md) for a detailed analysis.

## Documentation

| Document | Description |
|----------|-------------|
| [Server README](server/README.md) | API reference, WebSocket protocol, deployment |
| [MCP README](mcp/README.md) | MCP tool catalog, configuration, architecture |
| [Server config](server/sctl.toml.example) | Full TOML configuration reference |
| [MCP config](mcp/devices.example.json) | Multi-device JSON configuration example |
| [Security review](server/docs/REVIEW.md) | Security audit and known limitations |
| [Changelog](CHANGELOG.md) | Release history |
| [Contributing](CONTRIBUTING.md) | Development setup and guidelines |

## Requirements

- **Rust 1.82+** ([rustup.rs](https://rustup.rs/))
- **Docker** (for ARM cross-compilation via [cross](https://github.com/cross-rs/cross), optional)
- **Target device**: any Linux system with `/bin/sh` (tested on OpenWrt ARM and x86_64)

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup.

```bash
# Run all quality checks
cd server && make check
cd mcp && make check
```

## License

[GNU General Public License v3.0](LICENSE) -- Copyright (c) 2025 Alexandre Grenier
