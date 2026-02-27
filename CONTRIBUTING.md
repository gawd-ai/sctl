# Contributing to sctl

Thanks for your interest in contributing! This guide will help you get started.

Please read our [Code of Conduct](CODE_OF_CONDUCT.md) before contributing.

## Prerequisites

- Rust 1.82+ ([rustup](https://rustup.rs/))
- Docker (for ARM/RISC-V cross-compilation via [cross](https://github.com/cross-rs/cross), optional)
- Node.js 20+ (for the web UI, optional)
- jq (for `rundev.sh device` commands, optional)

## Project Structure

```
sctl/
├── server/        # sctl — HTTP/WebSocket server
│   └── src/
│       ├── routes/    # REST API handlers (exec, files, info, health, ...)
│       ├── tunnel/    # Relay and client tunnel (NAT traversal)
│       ├── sessions/  # Session management (PTY, persistence, AI status)
│       ├── gps.rs     # GPS tracking via Quectel modem GNSS
│       ├── lte.rs     # LTE signal monitoring via AT commands
│       ├── modem.rs   # Shared AT command infrastructure (serial port mutex)
│       ├── state.rs   # AppState, TunnelStats shared types
│       └── lib.rs     # Library crate re-exports
├── mcp/           # mcp-sctl — MCP proxy (runs on client side)
├── web/           # sctlin — Svelte 5 terminal UI component library
├── playbooks/     # Built-in playbook library
└── .github/       # CI workflows
```

The Rust crates are independent -- each has its own `Cargo.toml`, dependencies, and deployment target. The server targets embedded ARM devices; the MCP proxy runs on developer machines. The web package is a Svelte component library for terminal UIs.

## Development Setup

The fastest way to get a full working dev environment:

```bash
./rundev.sh
```

This builds the server, MCP proxy, and web UI, starts all services locally, and registers the MCP server with Claude Code.

### Other `rundev.sh` commands

```bash
./rundev.sh build    # Build only (no start/stop)
./rundev.sh start    # Restart services without rebuilding
./rundev.sh stop     # Stop all services + deregister MCP
./rundev.sh status   # Show what's running
./rundev.sh claude   # Only register MCP in Claude Code
./rundev.sh tunnel   # Start tunnel dev env (relay + physical devices)
```

### Device management

```bash
./rundev.sh device add <name> <host>   # Discover + register via SSH
./rundev.sh device ls                  # List devices with health status
./rundev.sh device deploy <name>       # Cross-compile + full deploy
./rundev.sh device upgrade <name>      # Binary-only upgrade
./rundev.sh device rm <name>           # Remove a device
```

Or run components individually:

```bash
# Terminal 1: start the server
cd server
SCTL_API_KEY=dev-key RUST_LOG=debug cargo run

# Terminal 2: verify it's running
curl http://localhost:1337/api/health
```

## Building

```bash
# Server
cd server && cargo build --release

# MCP proxy
cd mcp && cargo build --release

# Server for ARM (requires Docker)
cd server && make build-arm
```

## Quality Checks

Both Rust crates enforce the same standards:

```bash
cargo fmt --check              # Formatting
cargo clippy -- -D warnings    # Lints (zero warnings policy)
cargo test                     # Tests
cargo doc --no-deps            # Documentation builds
```

Each crate has a Makefile with a `check` target that runs all of the above:

```bash
cd server && make check
cd mcp && make check
```

CI runs these checks on every push and pull request.

## Code Style

- **Zero warnings** -- Clippy runs with `-D warnings` in CI
- **Formatting** -- standard `rustfmt`
- **Error handling** -- `?` propagation, descriptive messages at API boundaries
- **Comments** -- only where the logic isn't self-evident

## Pull Requests

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Ensure all checks pass (`make check` in both `server/` and `mcp/`)
4. Write a clear PR description explaining what and why
5. Link any related issues

## Reporting Issues

Open an issue at [github.com/gawd-ai/sctl/issues](https://github.com/gawd-ai/sctl/issues) with:

- Steps to reproduce
- Expected vs actual behavior
- sctl version (`curl /api/health`)
- Platform details

## Security Vulnerabilities

Please report security issues responsibly. See [SECURITY.md](SECURITY.md) for details.

## License

By contributing, you agree that your contributions will be licensed under the [GPL-3.0](LICENSE).
