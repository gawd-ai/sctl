# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-02-06

### Added

- **PTY support** — `session.start` accepts `pty: true` for full terminal emulation with ANSI escape codes, cursor movement, colors, and interactive TUI programs.
- **Session resize** — `session.resize` message to change PTY terminal dimensions (rows/cols).
- **Output journaling** — optional disk-backed persistence for session output, enabling crash recovery of persistent sessions. Configurable max age for automatic cleanup.
- **Session list** — `session.list` message to enumerate active sessions with status.

## [0.2.0] - 2026-02-06

### Added

- **Persistent sessions** — `session.start` gains `persistent: true` flag. Persistent sessions survive WebSocket disconnects; output keeps buffering for later re-attach.
- **Session re-attach** — new `session.attach` message type. Clients send `session_id` + `since` (last seen seq), server replays missed output from the ring buffer.
- **Process group signals** — new `session.signal` message type. Sessions are spawned with `setpgid(0, 0)`, signals sent to `-pgid` reach the entire process tree (real Ctrl-C behavior).
- **Buffer-backed sessions** — session output goes to an `OutputBuffer` ring buffer (configurable `session_buffer_size`, default 1000 entries) instead of being coupled directly to the WebSocket. A subscriber task forwards buffer entries to the WS.
- **Sequenced output** — `session.stdout`, `session.stderr`, and `session.system` messages now include `seq` (monotonic sequence number) and `timestamp_ms` fields for reliable ordering and catch-up.
- **Extended `session.start`** — now accepts `persistent`, `env`, and `shell` fields.
- **Config** — new `session_buffer_size` (default 1000) and `detach_timeout` (default 300s) settings in `[server]`.

### Changed

- **Session I/O architecture** — sessions write to `OutputBuffer` instead of directly to a WebSocket channel. A separate subscriber task reads from the buffer and forwards to WS. This decouples session lifetime from connection lifetime.
- **Disconnect behavior** — non-persistent sessions are killed on WS disconnect (backward compatible). Persistent sessions are detached instead.
- **Sweep task** — now also cleans up detached persistent sessions that exceed `detach_timeout`.

### Removed

- `src/ws/session.rs` — replaced by `src/sessions/session.rs` (buffer-backed `ManagedSession`).
- `src/ws/manager.rs` — replaced by `src/sessions/mod.rs` (new `SessionManager` with attach/detach).

## [0.1.0] - 2026-02-05

### Added

- **HTTP API** for remote device management:
  - `GET /api/health` — unauthenticated liveness probe (uptime, version)
  - `GET /api/info` — system introspection (hostname, kernel, CPU, memory, disk, network interfaces with IPs)
  - `POST /api/exec` — one-shot command execution with configurable timeout
  - `POST /api/exec/batch` — sequential batch command execution with per-command overrides
  - `GET /api/files` — read files (UTF-8 or base64) and list directories with symlink detection
  - `PUT /api/files` — atomic file writes (temp-then-rename) with optional mode and directory creation
- **WebSocket API** (`GET /api/ws`) for interactive shell sessions:
  - `session.start` / `session.kill` — lifecycle management
  - `session.exec` — send commands with acknowledgment
  - `session.stdin` — raw stdin input
  - `session.stdout` / `session.stderr` — chunk-based output streaming (4 KB, not line-buffered)
  - `session.exited` — process exit notification
  - `ping` / `pong` — keepalive
  - `request_id` correlation on all message types
- **Authentication** via pre-shared API key:
  - Bearer token for HTTP endpoints
  - Query parameter (`?token=`) for WebSocket upgrade
  - Constant-time comparison to prevent timing side-channels
- **Configuration** via TOML file with environment variable overrides:
  - `SCTL_API_KEY`, `SCTL_LISTEN`, `SCTL_DEVICE_SERIAL`
  - `--config <path>` CLI flag, falls back to `sctl.toml` in CWD
- **Resource limits** — `max_sessions`, `session_timeout`, `exec_timeout_ms`, `max_batch_size`, `max_file_size`
- **Security hardening**:
  - Path traversal prevention (rejects `..`, null bytes, relative paths)
  - `kill_on_drop` on all child processes
  - TOCTOU-safe session creation (write lock held across check-and-insert)
  - Pipe deadlock prevention (concurrent stdout/stderr reads, output drain past cap)
- **Graceful shutdown** — SIGINT/SIGTERM handling, kills all sessions
- **OpenWrt deployment** — procd init script, ARM cross-compilation via `cross`
