# sctl — AI Agent Guide

## Project Structure

```
sctl/
├── server/          # sctl daemon (Rust) — runs on target devices
├── mcp/             # mcp-sctl MCP proxy (Rust) — runs on dev machine
├── web/             # sctlin web terminal UI (Svelte 5 + Tailwind 4)
├── playbooks/       # Built-in playbook library (markdown + YAML)
├── docs/guide.md    # Deployment, fleet, tunnels, playbooks, GPS/LTE
└── rundev.sh        # Dev stack launcher + device management CLI
```

## Dev Stack

```bash
./rundev.sh              # build + start all (server + MCP + web UI)
./rundev.sh start        # restart without rebuilding
./rundev.sh stop         # stop everything
./rundev.sh status       # show running services
./rundev.sh claude       # register MCP in Claude Code only
```

### Device Management

```bash
./rundev.sh device add <name> <host>   # discover via SSH or sctl API
./rundev.sh device rm <name>           # remove device
./rundev.sh device ls                  # list with live health checks
./rundev.sh device deploy <name>       # deploy binary + config + init
./rundev.sh device upgrade <name>      # binary-only upgrade via SSH
```

`device add` probes the device, extracts arch/serial/api_key, and saves to
`~/.config/sctl/devices.dev.json`. It also regenerates `web/static/sctlin-seed.json`
so the web UI auto-discovers the device.

### Key Config Files

| File | Purpose |
|------|---------|
| `~/.config/sctl/devices.dev.json` | MCP device config (source of truth) |
| `web/static/sctlin-seed.json` | Auto-generated from devices config for web UI |
| `/etc/sctl/sctl.toml` | Server config on target devices |

The MCP proxy supports **hot-reload** — edit `devices.dev.json` and changes
are picked up on the next tool call. No restart needed.

## Building

```bash
# Individual components
cd server && cargo build --release
cd mcp && cargo build --release
cd web && npm run dev          # vite dev server at localhost:5173/sctlin

# Cross-compile server for embedded targets
cd server && cross build --release --target <target>
```

Supported targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`armv7-unknown-linux-musleabihf`, `riscv64gc-unknown-linux-gnu`

## Code Conventions

- **Rust edition 2021**, MSRV 1.82 (server) / 1.75 (MCP)
- Server uses axum + tokio; MCP uses tokio + serde_json for JSON-RPC 2.0
- Web uses Svelte 5 (runes, `$state`/`$derived`) + Tailwind CSS 4
- `cargo fmt` and `cargo clippy` before committing Rust changes
- Web: `npm run check` for svelte-check + TypeScript

## Working with Sessions (MCP)

When using sctl via MCP tools, prefer **sessions over device_exec** for anything
the user should see in the web terminal UI:

- `session_list` to find existing sessions
- `session_attach` to join a session the user opened in sctlin
- `session_exec_wait` for commands with finite output
- `session_exec` + `session_read` for long-running or streaming commands
- `device_exec` only when no session context exists or you need isolation

Sessions are shared between MCP and the sctlin web UI in real-time.
Working in a session lets the user watch your progress.

## Testing

```bash
cd server && cargo test
cd mcp && cargo test
cd web && npm test
```

## Architecture Notes

- **No eager device connections** — MCP proxy creates HTTP/WS connections on demand,
  not at startup. Unreachable devices don't slow down initialization.
- **WebSocket pool** — lazy, one connection per device, auto-reconnect on disconnect,
  cleaned up on config change.
- **Supervisor mode** — `mcp-sctl --supervisor` watches config file and respawns
  the MCP server on changes. Used by `rundev.sh claude`.
- **sctlin-seed.json** is derived from `devices.dev.json` by `generate_sctlin_seed`
  in rundev.sh. Never edit it manually — use `device add/rm` or re-run
  `./rundev.sh claude` to regenerate.
