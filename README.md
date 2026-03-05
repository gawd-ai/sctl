<p align="center">
  <img src="sctl-logo.png" alt="sctl" width="160" />
</p>

<h1 align="center">sctl</h1>

<p align="center">
  <strong>Shell Control — pronounced "scuttle" (yes, the Rustacean pun is intended).</strong><br/>
  Give AI agents hands-on access to Linux devices — routers, servers, IoT, VMs — across any network.<br/>
  Execute commands, manage persistent shell sessions, transfer files, run playbooks, track GPS and LTE — via <a href="https://modelcontextprotocol.io/">MCP</a>, HTTP/WebSocket API, or <a href="web/">web terminal UI</a>.
</p>

<p align="center">
  <a href="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml"><img src="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue.svg" alt="License: GPL-3.0" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.82%2B-orange.svg" alt="Rust 1.82+" /></a>
</p>

---

> **WARNING: SCTL IS AN EXTREMELY POWERFUL TOOL AND WITH GREAT POWER COMES GREAT RESPONSIBILITY. THIS TOOL CAN BE DANGEROUS AND CAUSE DESTRUCTION AND TOTAL OBLITERATION IF NOT USED WITH ABSOLUTE DILIGENCE. THE AUTHORS AND GAWD BEAR NO RESPONSIBILITY FOR ANY OUTCOME WHATSOEVER.**

---

## What can sctl do?

### AI troubleshoots a remote router

You ask Claude to check why a device is slow. The AI calls `device_exec` to check load, reads logs with `device_file_read`, spots a misconfiguration, and runs `pb_openwrt-health-check` to validate the fix — all on a device behind CGNAT, accessed through the tunnel relay.

```
You:   "The router at site-3 feels sluggish. Can you check it out?"

Claude: I'll check site-3. Let me start with system health.
        [calls device_exec on site-3: "uptime && free -m"]
        Load average is 4.2 on a 2-core device, and memory is at 93%.

        [calls device_exec: "ps aux --sort=-%mem | head -5"]
        dnsmasq is using 180MB — that's abnormal. Let me check its config.

        [calls device_file_read: "/etc/config/dhcp"]
        The DNS cache size is set to 50000 — way too high for 256MB RAM.

        [calls device_file_write: updates cachesize to 1000]
        [calls device_exec: "/etc/init.d/dnsmasq restart"]
        [calls pb_openwrt-health-check]

        Fixed. dnsmasq had a 50K entry DNS cache consuming most of RAM.
        Cache is now 1000 entries, memory usage dropped to 41%.
```

### Sessions survive network blips

Start a persistent PTY session on a device. Your WiFi drops. Your laptop sleeps. Hours later, reconnect — the session is still running, and every byte of output is waiting for you.

```
# Session starts, runs a long build
-> session.start  {persistent: true, pty: true, name: "build"}
<- session.started {session_id: "abc-123"}
-> session.exec   {command: "make -j4"}
<- session.stdout  {data: "Compiling...", seq: 0}

   ... network drops, laptop sleeps for 2 hours ...

# Reconnect — replay from where you left off
-> session.attach  {session_id: "abc-123", since: 0}
<- session.attached {entries: [seq 0..4847], dropped: 0}
   # All 4847 output entries replayed. Build finished at seq 4201.
```

### Fleet management across CGNAT

Manage devices on LAN and behind CGNAT from the same MCP config. The AI doesn't need to know which path each device uses — direct or tunneled, the API is identical.

```json
{
  "devices": {
    "office-router":  {"url": "http://192.168.1.1:1337",  "api_key": "..."},
    "cell-tower-42":  {"url": "https://relay.example.com/d/SCTL-0042", "api_key": "..."},
    "remote-sensor":  {"url": "https://relay.example.com/d/SCTL-0099", "api_key": "..."}
  },
  "default_device": "office-router"
}
```

```
You:   "Run a health check on all devices"

Claude: [calls device_health on office-router]    — ok, uptime 45d
        [calls device_health on cell-tower-42]     — ok, uptime 12d, via tunnel
        [calls device_health on remote-sensor]     — ok, uptime 3d, via tunnel
        [calls pb_linux-health-check on each]

        All 3 devices healthy. cell-tower-42 has disk at 87% — I'll clean
        old logs if you want.
```

## Architecture

```
                                                         ┌────────────────┐
┌────────────────┐  stdio (MCP)  ┌────────────────┐      │  sctl          │
│                │ <───────────> │                │ HTTP │  (device)      │
│    AI Agent    │  JSON-RPC 2.0 │   mcp-sctl     │ <──> │                │
│                │               │  Multi-device  │ +WS  │  Linux / ARM / │
└────────────────┘               │  Local buffers │      │  RISC-V / x86  │
                                 └────────────────┘      └────────────────┘
                                         │
┌────────────────┐  HTTP + WS            │           ┌────────────────┐
│   sctlin       │ <─────────────────────┘           │  sctl          │
│   (web UI)     │                            WS     │  (relay mode)  │
│   Terminal,    │ <──────────────────────────────>   │  NAT traversal │
│   files, etc.  │                                   │  for CGNAT     │
└────────────────┘                                   └────────────────┘
```

| Component | What it does |
|-----------|-------------|
| **[sctl](server/)** | Lightweight server on the target device — exec, sessions, files, GPS, LTE |
| **[mcp-sctl](mcp/)** | MCP proxy on your machine — translates AI tool calls into API requests |
| **[sctlin](web/)** | Svelte 5 web terminal — embeddable component library or standalone app |
| **sctl relay** | Same binary in relay mode — reverse tunnel for CGNAT/LTE devices |

## Key Capabilities

- **Persistent sessions** — shells survive disconnects, output buffers in a ring buffer, re-attach and catch up with zero loss
- **Full PTY support** — run vim, htop, docker — anything that needs a real terminal
- **Multi-device fleet** — manage many devices from one MCP server with hot-reloadable config
- **Reverse tunnel** — built-in NAT traversal for LTE/5G/CGNAT devices, with heartbeat, flap detection, and auto-reconnect
- **Playbooks** — markdown scripts with typed parameters, auto-discovered as MCP tools
- **GPS & LTE monitoring** — location tracking and signal metrics from Quectel modems, with autonomous watchdog recovery
- **AI collaboration** — session-level AI/human handoff, real-time working status in web UI
- **Security-first** — constant-time auth, path traversal prevention, process isolation, atomic writes

## Quick Start

### Option 1: Claude Code (recommended)

```bash
git clone https://github.com/gawd-ai/sctl.git && cd sctl

# Build everything and start the dev stack
chmod +x rundev.sh
./rundev.sh
```

This builds the server + MCP proxy, starts sctl locally, and registers it with Claude Code. Open a new Claude Code conversation and your AI can now execute commands, manage sessions, and operate your machine.

### Option 2: Manual Setup

**1. Start the server** on the target device:

```bash
cd server && cargo build --release
SCTL_API_KEY=your-secret-key ./target/release/sctl
# Listening on 0.0.0.0:1337
```

**2. Start the MCP proxy** on your dev machine:

```bash
cd mcp && cargo build --release
export SCTL_URL=http://your-device:1337
export SCTL_API_KEY=your-secret-key
claude mcp add sctl -- ./target/release/mcp-sctl
```

**3. Use it.** Ask Claude to run commands on your device — it will use `device_exec`, `session_start`, and other tools automatically.

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
| `device_file_delete` | Delete a file |
| `device_activity` | Read the activity log (exec, file I/O, sessions) |
| `device_gps` | GPS location data (fix, history, status) |
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
| `pb_*` | Auto-generated tools from playbooks (e.g. `pb_linux-health-check`) |

## Documentation

| Document | Description |
|----------|-------------|
| **[Guide](docs/guide.md)** | **Start here** — deployment, tunnel setup, playbooks, GPS/LTE, AI collaboration, troubleshooting |
| [Server README](server/README.md) | Full API reference, WebSocket protocol, TOML config |
| [MCP README](mcp/README.md) | MCP tool catalog, multi-device config, architecture |
| [Web README](web/README.md) | Component library API, widgets, integration examples |
| [Server config](server/sctl.toml.example) | Full TOML configuration reference |
| [MCP config](mcp/devices.example.json) | Multi-device JSON configuration example |
| [Security review](server/docs/REVIEW.md) | Security audit and known limitations |
| [Changelog](CHANGELOG.md) | Release history |
| [Contributing](CONTRIBUTING.md) | Development setup and guidelines |

## Requirements

- **Rust 1.82+** ([rustup.rs](https://rustup.rs/))
- **Docker** (for cross-compilation via [cross](https://github.com/cross-rs/cross), optional)
- **Node.js 20+** (for sctlin web UI, optional)
- **Target device**: any Linux system with `/bin/sh`

## License

[GNU General Public License v3.0](LICENSE) -- Copyright (c) 2025 Alexandre Grenier
