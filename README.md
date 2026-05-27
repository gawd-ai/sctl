<p align="center">
  <img src="sctl-logo.png" alt="sctl" width="160" />
</p>

<h1 align="center">sctl</h1>

<p align="center">
  <strong>Shell Control, pronounced "scuttle".</strong><br/>
  AI remote control plane for Linux devices, infrastructure, and edge compute.<br/>
  Give agents authenticated access to commands, persistent shell sessions, files, playbooks, GPS/LTE telemetry, and web terminals through <a href="https://modelcontextprotocol.io/">MCP</a>, HTTP/WebSocket APIs, or <a href="web/">sctlin</a>.
</p>

<p align="center">
  <a href="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml"><img src="https://github.com/gawd-ai/sctl/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue.svg" alt="License: GPL-3.0" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.82%2B-orange.svg" alt="Rust 1.82+" /></a>
</p>

---

> **Security warning:** sctl gives authenticated clients the ability to execute commands, write and delete files, manage shell sessions, and run playbooks on target systems. Run it only on systems you control, use strong API keys, restrict network exposure, and review commands and playbooks before granting AI access.

---

## Why sctl?

sctl exists to make AI agents useful outside the chat window. The first target can be your own computer, a lab machine, a server, or a VPS. The same control model extends to racks of Linux hosts, embedded development boards, communications and network hardware, drones, robots, and remote or space-based AI compute where shell access is the operational layer.

sctl does not provide domain-specific control logic for robots, drones, or spacecraft. It is a small, auditable control surface for Linux targets: commands, files, sessions, playbooks, telemetry, and relay access exposed in a form AI agents can use safely under operator control.

- **AI-native control surface** -- expose command execution, file operations, health checks, GPS, activity logs, and playbooks as MCP tools.
- **Durable interactive sessions** -- keep shells running on the target, replay missed output after reconnects, and support full PTY workflows.
- **Remote-first networking** -- use direct LAN URLs or the built-in relay for LTE/5G/CGNAT devices without changing client code.
- **Human/AI shared control** -- hand sessions between a web UI and an AI agent with per-session AI permission and visible working status.
- **Composable UI and APIs** -- use `sctlin` as a standalone app, embed it as a Svelte 5 component library, or integrate directly over HTTP/WebSocket.

## Where sctl Fits

- **Your own machine or lab computer** -- the base case is local: run sctl, connect an MCP-compatible agent, and let it inspect, build, test, or repair the system under your supervision.
- **Servers, VPSes, and cloud infrastructure** -- operate Linux hosts through the same MCP tools, REST API, WebSocket sessions, and playbooks.
- **Data centers and multi-device operations** -- manage named devices from one MCP server, keep persistent sessions alive, and use playbooks for repeatable operational checks.
- **Hardware development benches** -- bring AI control to boards, gateways, modems, and prototypes where the useful interface is still a shell, filesystem, serial-adjacent tooling, or device telemetry.
- **Communications and network devices** -- support OpenWrt and other Linux-based routers, LTE gateways, and remote network appliances that may sit behind CGNAT.
- **Robots, drones, and edge systems** -- give agents a controlled way to inspect and operate Linux-based compute modules without replacing the domain-specific flight, autonomy, or control software.
- **Space-based AI compute** -- apply the same relay, replayable session, telemetry, and playbook model to remote compute where links may be delayed, intermittent, or impossible to reach inbound.

## Example Workflows

**Inspect and repair a remote Linux host**

```text
Ask: "Check why site-3 is slow."
Agent: device_exec -> device_file_read -> pb_linux-health-check
Result: load, logs, disk state, config, and remediation steps are gathered from the host.
```

**Bring up embedded hardware behind LTE**

```text
Device: outbound tunnel over LTE/5G to a public relay
Agent: device_health -> device_gps -> pb_openwrt-diagnostics
Result: the board can be inspected and recovered without inbound network access.
```

**Persistent human/AI handoff**

```text
session_start    { persistent: true, pty: true, name: "field-debug" }
session_exec     { command: "make test" }
session_allow_ai { allowed: false }  # human takes over in sctlin
session_attach   { session_id: "...", since: 0 }  # agent replays missed output
```

**Same tools, direct or relayed**

```json
{
  "devices": {
    "office-router": { "url": "http://192.168.1.1:1337", "api_key": "..." },
    "lte-gateway": { "url": "https://relay.example.com/d/SCTL-0042", "api_key": "..." }
  },
  "default_device": "office-router"
}
```

## Architecture

```
┌──────────────┐   stdio/MCP    ┌──────────────┐   HTTP/WS    ┌──────────────┐
│  AI Agent    │ <------------> │  mcp-sctl    │ <----------> │ sctl device  │
│ MCP client   │  JSON-RPC 2.0  │ multi-device │              │ Linux shell  │
└──────────────┘                │ local buffer │              └──────────────┘
                                └──────┬───────┘
                                       │
                         HTTP/WS       │       WS tunnel
┌──────────────┐ <---------------------┘  ┌──────────────┐
│   sctlin     │                           │ sctl relay   │
│ web terminal │ <-----------------------> │ CGNAT/LTE    │
└──────────────┘                           └──────────────┘
```

| Component | What it does |
|-----------|-------------|
| **[sctl](server/)** | Device-side server for exec, sessions, files, activity, playbooks, GPS, and LTE |
| **[comms providers](drivers/)** | Target-specific helper binaries for hardware links such as the current Quectel LTE/GNSS provider |
| **[mcp-sctl](mcp/)** | MCP proxy that maps AI tool calls to sctl HTTP/WebSocket APIs |
| **[sctlin](web/)** | Svelte 5 web terminal and component library |
| **sctl relay** | Reverse tunnel mode for devices that cannot accept inbound connections |

## 0.5.0 Highlights

- **Unified error model** — stable API error codes and generated TypeScript bindings for client consistency.
- **Persistent PTY sessions** — replayable output buffers, resize support, process-group signals, and session journaling.
- **Playbook API and MCP tools** — Markdown playbooks with typed parameters exposed through REST and `pb_*` MCP tools.
- **Relay for CGNAT/LTE** — outbound device registration, proxied REST/WS sessions, heartbeat metrics, and reconnect handling.
- **External comms providers** — GPS/LTE telemetry, band control, and link recovery run through target-specific helper binaries so relay/VPS installs do not carry modem code.
- **sctlin web UI** — terminal, multi-server dashboard, activity history, playbooks, file tools, and embeddable widgets.

## Quick Start

### Agent Clients

```bash
git clone https://github.com/gawd-ai/sctl.git && cd sctl

# Build everything, start the dev stack, and register detected MCP clients
chmod +x rundev.sh
./rundev.sh
```

This is the base case: your own computer becomes the first controlled target. The script builds the server + MCP proxy, starts sctl locally, and registers `mcp-sctl` with detected MCP-compatible agent clients. Start a new agent session and it can execute commands, manage sessions, and operate the machine under your supervision.

Supported agent setup paths:

| Client | Command |
|--------|---------|
| Auto-detect installed clients | `./rundev.sh agents` |
| Claude Code | `./rundev.sh claude` |
| Codex CLI | `./rundev.sh codex` |
| Hermes | `./rundev.sh hermes` |
| OpenCode | `./rundev.sh opencode` |
| OpenClaw | `./rundev.sh openclaw` |
| Grok Build / generic MCP | `./rundev.sh grok` |
| NanoClaw / generic MCP | `./rundev.sh nanoclaw` |

Web UI: `http://localhost:5170/sctlin`.

#### Local development ports

`rundev.sh` probes `http://127.0.0.1:1337/api/health` on startup. If a healthy sctl server is already listening there, it attaches to that instance instead of launching a duplicate that would fail to bind the port. On `stop`, it only terminates processes it started.

Default local ports:
- sctl server: `127.0.0.1:1337`
- sctlin Vite UI: `http://localhost:5170/sctlin`

### Manual Setup

**1. Start the server** on the target device:

```bash
cd server && cargo build --release
SCTL_API_KEY=your-secret-key ./target/release/sctl
# Listening on 0.0.0.0:1337
```

**2. Register the MCP proxy** with your agent client:

```bash
cd mcp && cargo build --release

# Example: any client that accepts a stdio MCP command can launch:
./target/release/mcp-sctl --config ../mcp/devices.example.json
```

**3. Use it.** Ask your agent to run commands on your device. It will use `device_exec`, `session_start`, and other tools automatically when connected through MCP.

## MCP Capabilities

When connected through `mcp-sctl`, agents get tools for:

- Device discovery, health, info, command execution, and activity history.
- Atomic file read/write/delete operations.
- Persistent session lifecycle, raw terminal input, signals, resize, attach, and wait helpers.
- AI collaboration controls such as `session_allow_ai` and `session_ai_status`.
- Playbook CRUD and auto-generated `pb_*` tools from device-stored Markdown playbooks.

See [mcp/README.md](mcp/README.md) for the complete tool catalog and config format.

## Documentation

| Document | Description |
|----------|-------------|
| **[Guide](docs/guide.md)** | Deployment, relay setup, playbooks, GPS/LTE, AI collaboration, troubleshooting |
| [MCP README](mcp/README.md) | MCP configuration, full tool catalog, multi-device behavior |
| [Server README](server/README.md) | HTTP API, WebSocket protocol, TOML config, relay API |
| [Web README](web/README.md) | sctlin package API, components, widgets, integration examples |
| [Server config](server/sctl.toml.example) | Full TOML configuration reference |
| [MCP config](mcp/devices.example.json) | Multi-device JSON configuration example |
| [Security policy](SECURITY.md) | Vulnerability reporting and supported versions |
| [Security review](server/docs/REVIEW.md) | Security design notes and known limitations |
| [Changelog](CHANGELOG.md) | Release history |
| [Contributing](CONTRIBUTING.md) | Development setup and guidelines |

## Requirements

- **Rust 1.82+** ([rustup.rs](https://rustup.rs/))
- **Docker** (for cross-compilation via [cross](https://github.com/cross-rs/cross), optional)
- **Node.js 20+** (for sctlin web UI, optional)
- **Target device**: any Linux system with `/bin/sh`

## License

[GNU General Public License v3.0](LICENSE) -- Copyright (c) 2025 Alexandre Grenier
