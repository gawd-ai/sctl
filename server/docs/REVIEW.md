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
