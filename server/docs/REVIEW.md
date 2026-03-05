# sctl — Security & Code Review

Review of the sctl codebase as of v0.3.0.

## Security Wins

### Constant-time auth comparison (`auth.rs`)

The `constant_time_eq` function iterates the full expected-key length regardless of input, preventing timing side-channel attacks.

### Path traversal prevention (`routes/files.rs`)

`validate_path` rejects `..` at the component level, null bytes, and relative paths. Validation happens at the API boundary before any filesystem access.

### Session limits (`sessions/mod.rs`)

Hard cap on concurrent sessions prevents resource exhaustion. The write-lock is held across the check-and-insert to prevent TOCTOU races.

### Process isolation

- `kill_on_drop(true)` on all child processes — no orphaned processes if the server crashes
- Sessions spawned in own process group (`setpgid(0, 0)`) — signals reach the entire process tree
- Output past the 1 MB cap is drained (not pipe-closed) to avoid SIGPIPE

### Pipe deadlock prevention (`shell/process.rs`)

Stdout and stderr are read concurrently via `tokio::join!`. Reading one then the other can deadlock when the OS pipe buffer fills.

### Atomic file writes (`routes/files.rs`)

Write-to-temp-then-rename with an atomic counter for unique temp names. Readers never see partial content. Concurrent writes are safe.

## Known Limitations

These are tracked for future work and are low-risk in current deployments.

1. **`max_connections` configured but not enforced** — the setting exists in config but no middleware limits concurrent connections. Could be enforced via Tower's `ConcurrencyLimit` layer.

2. **Session timeout is wall-clock, not idle-based** — sessions are killed after a fixed duration from creation, regardless of activity. An idle-based timeout would be better for long-running interactive sessions.

3. **No explicit request body size limit** — relies on Axum's default 2 MB body limit. Should be explicitly configured to match `max_file_size`.

## v0.4.0 Additions

### Tunnel security

The reverse tunnel uses a shared `tunnel_key` for device-to-relay authentication. When a device registers, the relay learns its `api_key`, which is then required for client-to-relay requests routed to that device. REST requests are translated to JSON messages over the device's outbound WebSocket connection and routed back by `request_id`.

**Limitations**: The `tunnel_key` is a single shared secret — all devices that connect to a relay share the same key. A compromised key allows any device to register. There is no per-device tunnel authentication or certificate pinning. The relay trusts the device's self-reported serial number.

### Playbook security

Playbooks are Markdown files with YAML frontmatter (params) and a fenced shell script. Template parameters (`{{param}}`) are substituted server-side before execution. There is no sandboxing or allow-listing of commands — playbooks run as the sctl process user with full shell access. Parameter values are injected directly into the script via string substitution; the server does not validate or sanitize parameter values at execution time.

### Modem/GPS security

GPS and LTE monitoring use AT commands sent over a serial port (`/dev/ttyUSB2`). There is no filtering or allow-listing of AT commands — the modem interface is accessed by the polling tasks, not exposed directly via API. The GPS and LTE API endpoints return parsed data, not raw AT responses.

### AI status trust model

The `session.ai_status` and `session.allow_ai` messages are informational coordination signals for UI display. They are not a security boundary — any authenticated WebSocket client can set AI status or toggle AI permission on any session. These are designed for collaborative human/AI workflows, not access control.
