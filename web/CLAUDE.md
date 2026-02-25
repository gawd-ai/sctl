# sctlin — AI Integration Guide

This file is designed for AI agents (Claude Code, Cursor, etc.) to integrate sctlin without reading source code.

## Package Overview

**sctlin** is a Svelte 5 component library for connecting to [sctl](https://github.com/gawd-coder/sctl) daemons — lightweight device management agents running on Linux/OpenWrt devices. sctlin provides:

- Terminal emulation (xterm.js) with session management, split panes, tabs
- Device info, activity monitoring, playbook execution widgets
- WebSocket + REST clients for programmatic access
- Chunked file transfer engine (STP protocol) with progress and SHA-256 verification

**Package name**: `sctlin`
**Requires**: Svelte 5 (`svelte ^5.0.0`), Tailwind CSS 4
**Protocol**: WebSocket (sessions, real-time events) + HTTP REST (device info, files, exec, playbooks, transfers)

### Import Paths

```ts
import { TerminalContainer, Terminal, ... } from 'sctlin/components';
import { TerminalWidget, DeviceStatusWidget, ActivityWidget, PlaybookWidget } from 'sctlin/widgets';
import type { SctlinConfig, SessionInfo, DeviceInfo, ... } from 'sctlin/types';
import { SctlWsClient, SctlRestClient, ConnectionManager, TransferTracker, SctlError, ... } from 'sctlin/utils';
```

### Tailwind CSS Requirement

sctlin components use Tailwind CSS 4 utility classes. The consuming app MUST add:

```css
/* app.css */
@import 'tailwindcss';
@source '../node_modules/sctlin/dist';
```

Without `@source`, components render unstyled.

---

## Quick Integration Recipes

### Embed a Terminal in 10 Lines

```svelte
<script>
  import { TerminalWidget } from 'sctlin/widgets';
</script>

<div class="h-screen">
  <TerminalWidget
    config={{ wsUrl: 'ws://device:1337/api/ws', apiKey: 'your-key' }}
  />
</div>
```

`TerminalWidget` handles connection, session creation, and cleanup automatically.

### Connect to a Device and Show Status

```svelte
<script>
  import { DeviceStatusWidget } from 'sctlin/widgets';
</script>

<DeviceStatusWidget
  config={{ wsUrl: 'ws://device:1337/api/ws', apiKey: 'your-key' }}
  pollInterval={15000}
/>
```

### Full Dashboard with Multi-Server

```svelte
<script>
  import { ConnectionManager } from 'sctlin/utils';
  import { TerminalContainer } from 'sctlin/components';

  const servers = [
    { id: 'dev', name: 'Dev Box', wsUrl: 'ws://dev:1337/api/ws', apiKey: 'k1', shell: '' },
    { id: 'prod', name: 'Production', wsUrl: 'ws://prod:1337/api/ws', apiKey: 'k2', shell: '' },
  ];

  let activeServer = $state(servers[0]);
  let sessions = $state([]);

  const manager = new ConnectionManager({}, {
    onConnectionChange: (id, status) => { /* update UI */ },
    onDeviceInfo: (id, info) => { /* update device panel */ },
  });

  for (const s of servers) manager.connect(s);

  let config = $derived(manager.buildSctlinConfig(activeServer, {
    onSessionsChange: (s) => { sessions = s; },
  }));
</script>

<nav>
  {#each servers as s}
    <button onclick={() => activeServer = s}>{s.name}</button>
  {/each}
</nav>
<TerminalContainer {config} />
```

---

## API Reference

### ConnectionManager

Orchestrates multi-server connections. Creates and manages WS/REST clients per server.

```ts
import { ConnectionManager } from 'sctlin/utils';

const manager = new ConnectionManager(config?, events?);
```

**Constructor params:**

| Param | Type | Description |
|-------|------|-------------|
| `config.httpTimeout` | `number` | REST timeout (ms). Default: 30000 |
| `config.pingInterval` | `number` | WS keepalive interval (ms). Default: 30000 |
| `config.ackTimeout` | `number` | WS ack timeout (ms). Default: 10000 |
| `config.maxActivityEntries` | `number` | Activity cap per server. Default: 200 |
| `config.autoFetchInfo` | `boolean` | Fetch device info on connect. Default: true |
| `config.autoFetchActivity` | `boolean` | Fetch activity on connect. Default: true |
| `config.reconnect` | `Partial<ReconnectConfig>` | Reconnect behavior |
| `config.sessionDefaults` | `Partial<SessionStartOptions>` | Default session options |

**Methods:**

| Method | Returns | Description |
|--------|---------|-------------|
| `connect(server)` | `ServerConnection` | Connect to a server (idempotent) |
| `disconnect(serverId)` | `void` | Disconnect one server |
| `get(serverId)` | `ServerConnection?` | Get connection by ID |
| `getAll()` | `ServerConnection[]` | All active connections |
| `buildSctlinConfig(server, callbacks?)` | `SctlinConfig` | Build config for TerminalContainer |
| `fetchDeviceInfo(serverId)` | `Promise<DeviceInfo?>` | Fetch device info |
| `fetchActivity(serverId, sinceId?, limit?)` | `Promise<ActivityEntry[]>` | Fetch activity log |
| `disconnectAll()` | `void` | Disconnect all servers |
| `destroy()` | `void` | Disconnect all + release resources |

**Events (second constructor param):**

| Event | Signature |
|-------|-----------|
| `onConnectionChange` | `(serverId: string, status: ConnectionStatus) => void` |
| `onDeviceInfo` | `(serverId: string, info: DeviceInfo \| null) => void` |
| `onActivity` | `(serverId: string, entries: ActivityEntry[]) => void` |
| `onActivityNew` | `(serverId: string, entry: ActivityEntry) => void` |
| `onTransferChange` | `(serverId: string, transfers: ClientTransfer[]) => void` |
| `onTransferError` | `(serverId: string, transfer: ClientTransfer, message: string) => void` |
| `onError` | `(serverId: string, error: Error) => void` |

### TerminalContainer

Main terminal UI component. Use `bind:this` for imperative control.

**Props:**

| Prop | Type | Default | Description |
|------|------|---------|-------------|
| `config` | `SctlinConfig` | required | Connection, session defaults, callbacks |
| `showTabs` | `boolean` | `true` | Show session tab bar |
| `onToggleFiles` | `() => void` | — | File browser toggle callback |
| `onTogglePlaybooks` | `() => void` | — | Playbooks toggle callback |
| `sidePanelOpen` | `boolean` | `false` | Side panel open state |
| `sidePanelTab` | `string` | `''` | Active side panel tab |
| `rightInset` | `number` | `0` | Right margin in px (for side panel) |
| `rightInsetAnimate` | `boolean` | `false` | Animate right inset transitions |

**Key exported methods (grouped):**

Session lifecycle: `startSession(shell?)`, `attachSession(sessionId)`, `listShells()`, `closeSession(key)`, `killSessionById(sessionId)`, `detachSession(key)`, `closeTab(key)`, `renameSession(key, label)`

Navigation: `selectSession(key)`, `getActiveKey()`, `getSessionList()`

Split panes: `splitHorizontal()`, `splitVertical()`, `unsplit()`, `toggleSplitFocus()`, `getSplitPrimaryKey()`, `getSplitSecondaryKey()`, `getSplitGroups()`

UI: `toggleSearch()`

AI: `setAllAi(allowed)`

Execution: `execInActiveSession(command)`, `exec(command)`

Remote: `getRemoteSessions()`, `fetchRemoteSessions()`

### Widgets

Each widget accepts a `DeviceConnectionConfig` and manages its own connections.

**TerminalWidget** — Self-contained terminal with auto-session.

| Prop | Type | Default |
|------|------|---------|
| `config` | `DeviceConnectionConfig` | required |
| `theme` | `TerminalTheme` | xterm defaults |
| `showTabs` | `boolean` | `true` |
| `class` | `string` | `''` |

**DeviceStatusWidget** — Device info with periodic polling.

| Prop | Type | Default |
|------|------|---------|
| `config` | `DeviceConnectionConfig` | required |
| `pollInterval` | `number` | `30000` |
| `class` | `string` | `''` |

**ActivityWidget** — Activity feed with optional real-time WS updates.

| Prop | Type | Default |
|------|------|---------|
| `config` | `DeviceConnectionConfig` | required |
| `maxEntries` | `number` | `100` |
| `realtime` | `boolean` | `true` |
| `class` | `string` | `''` |

**PlaybookWidget** — Playbook browser, viewer, and executor.

| Prop | Type | Default |
|------|------|---------|
| `config` | `DeviceConnectionConfig` | required |
| `editable` | `boolean` | `false` |
| `class` | `string` | `''` |

### SctlWsClient

WebSocket client with reconnect, typed events, and request/ack correlation.

```ts
const ws = new SctlWsClient(wsUrl, apiKey, reconnect?, config?);
ws.connect();
```

**Key methods:**

| Method | Returns | Description |
|--------|---------|-------------|
| `connect()` | `void` | Open WebSocket (no-op if already connected) |
| `disconnect()` | `void` | Close + cancel reconnect + reject pending acks |
| `onStatusChange(cb)` | `() => void` | Subscribe to status changes (returns unsub fn) |
| `on(type, cb)` | `() => void` | Subscribe to a message type (returns unsub fn) |
| `onOutput(sessionId, cb)` | `() => void` | Subscribe to session output (stdout/stderr/system) |
| `onSessionEnd(sessionId, cb)` | `() => void` | Subscribe to session close/exit |
| `startSession(opts?)` | `Promise<WsSessionStartedMsg>` | Start a shell session |
| `attachSession(sessionId, since?)` | `Promise<WsSessionAttachedMsg>` | Attach to existing session |
| `killSession(sessionId)` | `Promise<WsSessionClosedMsg>` | Kill a session |
| `sendStdin(sessionId, data)` | `void` | Send keystrokes (fire-and-forget) |
| `execCommand(sessionId, command)` | `Promise<void>` | Execute command in session |
| `sendSignal(sessionId, signal)` | `Promise<void>` | Send POSIX signal |
| `resizeSession(sessionId, rows, cols)` | `Promise<...>` | Resize PTY |
| `listSessions()` | `Promise<WsSessionListedMsg>` | List server sessions |
| `listShells()` | `Promise<WsShellListedMsg>` | List available shells |
| `renameSession(sessionId, name)` | `Promise<...>` | Rename session |
| `setUserAllowsAi(sessionId, allowed)` | `Promise<...>` | Set AI permission |

### SctlRestClient

HTTP client. Derives base URL from WS URL (`ws://host:port/api/ws` → `http://host:port`).

```ts
const rest = new SctlRestClient(wsUrl, apiKey, config?);
```

**Key methods:**

| Method | Returns | Description |
|--------|---------|-------------|
| `getInfo()` | `Promise<DeviceInfo>` | Device system info |
| `getHealth()` | `Promise<{status, uptime, version}>` | Health check (no auth) |
| `listDir(path)` | `Promise<DirEntry[]>` | List directory |
| `readFile(path, opts?)` | `Promise<FileContent>` | Read file content |
| `writeFile(path, content, opts?)` | `Promise<void>` | Write file |
| `deleteFile(path)` | `Promise<void>` | Delete file |
| `exec(command, opts?)` | `Promise<ExecResult>` | One-shot command execution |
| `getActivity(sinceId?, limit?)` | `Promise<ActivityEntry[]>` | Activity log |
| `listPlaybooks()` | `Promise<PlaybookSummary[]>` | List playbooks |
| `getPlaybook(name)` | `Promise<PlaybookDetail>` | Get playbook detail |
| `putPlaybook(name, content)` | `Promise<void>` | Create/update playbook |
| `deletePlaybook(name)` | `Promise<void>` | Delete playbook |
| `downloadUrl(path)` | `string` | Raw download URL |
| `downloadBlob(path)` | `Promise<{blob, filename}>` | Download as Blob |
| `uploadFiles(dirPath, files)` | `Promise<void>` | Multipart upload |
| `stpInitDownload(path)` | `Promise<StpInitDownloadResult>` | Start STP download |
| `stpInitUpload(req)` | `Promise<StpInitUploadResult>` | Start STP upload |
| `stpGetChunk(transferId, index)` | `Promise<{data, hash}>` | Download chunk |
| `stpSendChunk(transferId, index, data, hash)` | `Promise<StpChunkAck>` | Upload chunk |
| `stpAbort(transferId)` | `Promise<void>` | Abort transfer |

### Error Classes

```
SctlError (base — has .code: string, .message: string)
├── ConnectionError    — WebSocket not connected (code: 'connection_error')
├── ServerError        — Server responded with error (code: server-provided)
├── TimeoutError       — Operation timed out (code: 'timeout')
├── HttpError          — HTTP non-OK response (code: 'http_error', .status: number, .body: string)
└── TransferError      — File transfer failed (code: 'transfer_error', .transferId?: string)
```

All importable from `sctlin/utils`.

---

## Type Shapes

### ServerConfig

```ts
interface ServerConfig {
  id: string;       // Unique identifier
  name: string;     // Display name
  wsUrl: string;    // WebSocket URL (e.g. 'ws://host:1337/api/ws')
  apiKey: string;   // Bearer token
  shell: string;    // Preferred shell ('' = device default)
}
```

### SctlinConfig

```ts
interface SctlinConfig {
  wsUrl: string;
  apiKey: string;
  theme?: TerminalTheme;
  defaultRows?: number;           // default: 24
  defaultCols?: number;           // default: 80
  autoConnect?: boolean;          // default: true
  autoStartSession?: boolean;     // default: true
  reconnect?: Partial<ReconnectConfig>;
  callbacks?: SctlinCallbacks;
  sessionDefaults?: Partial<SessionStartOptions>;
  client?: SctlWsClient;         // pre-created WS client
}
```

### SctlinCallbacks

```ts
interface SctlinCallbacks {
  onConnectionChange?: (status: ConnectionStatus) => void;
  onSessionStarted?: (session: SessionInfo) => void;
  onSessionClosed?: (sessionId: string, reason: string) => void;
  onSessionsChange?: (sessions: SessionInfo[]) => void;
  onActiveSessionChange?: (key: string | null) => void;
  onRemoteSessions?: (sessions: RemoteSessionInfo[]) => void;
  onAiPermissionChange?: (sessionId: string, allowed: boolean) => void;
  onAiStatusChange?: (sessionId: string, working: boolean, activity?: string, message?: string) => void;
  onSplitGroupsChange?: (groups: SplitGroupInfo[]) => void;
  onFocusedPaneChange?: (pane: 'primary' | 'secondary') => void;
  onResize?: (sessionId: string, rows: number, cols: number) => void;
  onError?: (error: WsErrorMsg) => void;
  onActivity?: (entry: ActivityEntry) => void;
}
```

### DeviceConnectionConfig

```ts
interface DeviceConnectionConfig {
  wsUrl: string;           // WebSocket URL
  apiKey: string;          // Bearer token
  autoConnect?: boolean;   // default: true
}
```

### SessionInfo

```ts
interface SessionInfo {
  key: string;             // Client-generated UUID (local tab identifier)
  sessionId: string;       // Server-assigned session ID
  pid?: number;
  persistent: boolean;
  pty: boolean;
  userAllowsAi: boolean;
  aiIsWorking: boolean;
  aiActivity?: string;     // 'read' | 'write'
  aiStatusMessage?: string;
  lastSeq: number;         // Last output sequence number
  label?: string;          // Display name
  attached: boolean;       // Receiving output
  serverId?: string;       // Multi-server mode
  serverName?: string;
  dead?: boolean;          // Server session disappeared
}
```

### TerminalTheme

```ts
interface TerminalTheme {
  // UI colors
  background?: string;
  foreground?: string;
  cursor?: string;
  cursorAccent?: string;
  selectionBackground?: string;
  selectionForeground?: string;
  selectionInactiveBackground?: string;
  // ANSI standard colors (0–7)
  black?: string; red?: string; green?: string; yellow?: string;
  blue?: string; magenta?: string; cyan?: string; white?: string;
  // ANSI bright colors (8–15)
  brightBlack?: string; brightRed?: string; brightGreen?: string; brightYellow?: string;
  brightBlue?: string; brightMagenta?: string; brightCyan?: string; brightWhite?: string;
  // Font
  fontFamily?: string;
  fontSize?: number;
}
```

### ReconnectConfig

```ts
interface ReconnectConfig {
  enabled: boolean;       // default: true
  initialDelay: number;   // default: 100 (ms)
  maxDelay: number;       // default: 2000 (ms)
  maxAttempts: number;    // default: Infinity
}
```

---

## Common Patterns

### Callback → Reactivity Bridge

sctlin uses plain callbacks, not framework stores. Bridge them to your reactivity system:

**Svelte 5:**
```svelte
<script>
  let sessions = $state([]);
  let status = $state('disconnected');

  const config = {
    wsUrl: '...', apiKey: '...',
    callbacks: {
      onSessionsChange: (s) => { sessions = s; },
      onConnectionChange: (s) => { status = s; },
    }
  };
</script>
```

**React (conceptual):**
```tsx
const [sessions, setSessions] = useState([]);
const config = useMemo(() => ({
  wsUrl: '...', apiKey: '...',
  callbacks: { onSessionsChange: setSessions }
}), []);
```

### Multi-Server State Management

Per-connection state uses `Record<serverId, T>`:

```ts
let deviceInfo: Record<string, DeviceInfo | null> = $state({});
let activity: Record<string, ActivityEntry[]> = $state({});

const manager = new ConnectionManager({}, {
  onDeviceInfo: (id, info) => { deviceInfo[id] = info; deviceInfo = deviceInfo; },
  onActivity: (id, entries) => { activity[id] = entries; activity = activity; },
});
```

### Session Lifecycle: Start → Use → Detach vs Close vs Kill

- **`startSession()`** — Creates a new server session + local tab
- **`detachSession(key)`** — Removes local tab, server session keeps running (can re-attach later)
- **`closeTab(key)`** — Same as detach (removes tab, session continues)
- **`closeSession(key)`** — Removes tab AND kills server session
- **`killSessionById(sessionId)`** — Kills server session by ID (even without a local tab)

### File Transfers

```ts
const conn = manager.get('server-id');
const tracker = conn.transferTracker;

// Download
const blob = await tracker.download('/var/log/syslog', (p) => {
  console.log(`${Math.round(p.fraction * 100)}%`);
});
saveAs(blob, 'syslog');

// Upload
await tracker.upload('/tmp', file, (p) => {
  console.log(`${Math.round(p.fraction * 100)}%`);
});

// Abort
tracker.abort(transferId);
```

### Handling Reconnection

On WebSocket disconnect, sctlin auto-reconnects with exponential backoff. On reconnect:

1. `TerminalContainer` fetches the server's session list
2. Compares against local sessions — missing sessions are marked `dead: true`
3. Surviving sessions are re-attached (output replayed from last sequence number)
4. Dead sessions show a "Session Lost" overlay with disabled input

---

## Gotchas

1. **Tailwind `@source` is required** — Without `@source '../node_modules/sctlin/dist'` in your CSS, all components render as unstyled divs.

2. **`SctlinConfig.client`** — Pass a pre-created `SctlWsClient` to share connections. `ConnectionManager.buildSctlinConfig()` does this automatically. Without it, `TerminalContainer` creates its own WS client.

3. **`SessionInfo.key` vs `SessionInfo.sessionId`** — `key` is a client-generated UUID for tab/pane management. `sessionId` is the server-assigned ID. Methods on `TerminalContainer` accept `key`; methods on `SctlWsClient` accept `sessionId`.

4. **`SessionInfo.dead`** — `true` when the server session disappeared (e.g. device reboot). Dead sessions cannot be re-attached. The UI shows a gray overlay.

5. **Widget `autoConnect` defaults to true** — Widgets connect immediately on mount. Set `autoConnect: false` to defer connection.

6. **`TerminalContainer` does NOT auto-start a session by default** — `autoStartSession` defaults to `true` in `SctlinConfig`, but `ConnectionManager.buildSctlinConfig()` sets it to `false` (expects the consumer to manage session lifecycle). When using `TerminalContainer` directly, the default is `true`.

7. **WS URL format** — Must end with `/api/ws` (e.g. `ws://host:1337/api/ws`). The REST client derives its HTTP base URL by stripping `/api/ws`.

8. **All methods that accept `key` are local identifiers** — Use `getSessionList()` to map between `key` and `sessionId`. The `key` is ephemeral (per-page-load); the `sessionId` persists across reconnects.
