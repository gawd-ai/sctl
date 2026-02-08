# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-02-06

### sctl (server)

#### Added

- **PTY support** — `session.start` accepts `pty: true` for full terminal emulation with ANSI escape codes, cursor movement, colors, and interactive TUI programs.
- **Session resize** — `session.resize` message to change PTY terminal dimensions (rows/cols).
- **Output journaling** — optional disk-backed persistence for session output, enabling crash recovery of persistent sessions. Configurable max age for automatic cleanup.
- **Session list** — `session.list` message to enumerate active sessions with status.

### mcp-sctl (MCP proxy) — initial release

#### Added

- **Device tools** (HTTP): `device_list`, `device_health`, `device_info`, `device_exec`, `device_exec_batch`, `device_file_read`, `device_file_write`.
- **Session tools** (WebSocket): `session_start`, `session_exec`, `session_send`, `session_read`, `session_signal`, `session_kill`, `session_attach`, `session_resize`, `session_exec_wait`, `session_list`.
- **Multi-device support** — JSON config with named devices and per-device API keys.
- **Local output buffering** — session output cached in-process for zero-latency reads.
- **Auto-reconnect** — WebSocket disconnects trigger exponential backoff reconnect with sequence-based re-attach. No output lost.
- **Playbook discovery** — device-stored markdown playbooks automatically exposed as dynamic MCP tools (`pb_*`).
- **`session_exec_wait`** — execute a command and wait for completion in a single call using marker-based detection.
- **Claude Code integration** — `dev-setup.sh` for one-command development environment setup.

## [0.2.0] - 2026-02-06

### sctl (server)

#### Added

- **Persistent sessions** — `session.start` gains `persistent: true` flag. Persistent sessions survive WebSocket disconnects; output keeps buffering for later re-attach.
- **Session re-attach** — `session.attach` message. Clients send `session_id` + `since` (last seen seq), server replays missed output from the ring buffer.
- **Process group signals** — `session.signal` message. Sessions spawned with `setpgid(0, 0)`, signals sent to `-pgid` reach the entire process tree (real Ctrl-C).
- **Buffer-backed sessions** — output goes to `OutputBuffer` ring buffer (configurable, default 1000 entries) instead of being coupled to the WebSocket.
- **Sequenced output** — `session.stdout`, `session.stderr`, `session.system` include `seq` and `timestamp_ms` for reliable ordering and catch-up.
- **Config** — `session_buffer_size` and `detach_timeout` settings.

#### Changed

- Session I/O decoupled from WebSocket — output goes to buffer, subscriber task forwards to WS.
- Non-persistent sessions killed on WS disconnect (backward compatible). Persistent sessions detached.
- Sweep task cleans up detached persistent sessions past `detach_timeout`.

## [0.1.0] - 2026-02-05

### sctl (server)

#### Added

- **HTTP API**: health, system info, command execution (single + batch), file read/write with symlink detection.
- **WebSocket API**: interactive shell sessions with start/kill, exec, stdin, streaming stdout/stderr, exit notification, ping/pong, request_id correlation.
- **Authentication**: pre-shared API key with constant-time comparison, Bearer header for HTTP, query param for WebSocket.
- **Configuration**: TOML file with environment variable overrides.
- **Resource limits**: max_sessions, session_timeout, exec_timeout_ms, max_batch_size, max_file_size.
- **Security**: path traversal prevention, kill_on_drop, TOCTOU-safe session creation, pipe deadlock prevention, atomic file writes.
- **Graceful shutdown**: SIGINT/SIGTERM handling, clean session teardown.
- **OpenWrt deployment**: procd init script, ARM cross-compilation via `cross`, Makefile deploy target.
