# Contributing to sctl

Thanks for your interest in contributing! This guide will help you get started.

Please read our [Code of Conduct](CODE_OF_CONDUCT.md) before contributing.

## Prerequisites

- Rust 1.75+ ([rustup](https://rustup.rs/))
- Docker (for ARM cross-compilation only)
- Node.js 20+ (for the web UI, optional)

## Project Structure

```
sctl/
├── server/    # sctl — HTTP/WebSocket server (runs on target devices)
├── mcp/       # mcp-sctl — MCP proxy (runs on client side)
├── web/       # sctlin — Svelte 5 terminal UI component library
└── .github/   # CI workflows
```

The Rust crates are independent -- each has its own `Cargo.toml`, dependencies, and deployment target. The server targets embedded ARM devices; the MCP proxy runs on developer machines. The web package is a Svelte component library for terminal UIs.

## Development Setup

The fastest way to get a full working dev environment:

```bash
./rundev.sh
```

This builds the server, MCP proxy, and web UI, starts all services locally, and registers the MCP server with Claude Code.

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
