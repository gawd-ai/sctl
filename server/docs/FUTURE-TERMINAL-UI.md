# Web Terminal UI

Design notes for the browser-based terminal that connects to sctl PTY sessions.

The initial implementation lives in [`web/`](../../web/) as the `sctlin` Svelte 5 component library.

## Architecture

```
Browser (xterm.js)  <---- WebSocket ---->  sctl PTY session
                     JSON frames for       (server/src/ws/)
                     session I/O
```

## Key Concepts

### xterm.js Integration
- xterm.js in the browser connects via WebSocket to sctl
- Terminal I/O is sent as JSON WebSocket messages (session.stdin / session.stdout)
- The terminal size (rows/cols) is synced via `session.resize` messages

### Multiplexing
- Multiple viewers can observe a session simultaneously (read-only)
- Each viewer gets their own subscriber to the session's output buffer
- Viewers see real-time output as it arrives

### Controller Role
- Only one writer at a time controls the session
- AI hands off to human, human hands back
- Protocol message: `session.allow_ai { session_id, allow: true|false }`
- When AI is disabled, the browser has exclusive input
- When AI is enabled, the browser is read-only

### Session Handoff Flow

```
1. AI starts PTY session via MCP (session_start with pty=true)
2. AI works in session (session_exec, session_send)
3. Human opens web UI, connects to same session
4. Human takes control (disables AI input)
5. Human interacts directly with the terminal
6. Human finishes, returns control to AI
7. AI resumes, reads any output produced during human control
```

### Implementation Notes
- sctl supports PTY sessions and resize (v0.3.0)
- xterm.js handles all terminal rendering and input
- No server-side screen buffer needed -- xterm.js maintains terminal state
- Web fonts (MesloLGS NF) must be loaded before first `fitAddon.fit()` call
- Resize must be debounced (~200ms) to prevent garbled output with complex prompts
