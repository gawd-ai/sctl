# sctlin

Svelte 5 component library for [sctl](../README.md) — embeddable terminal, device management, playbook execution, and activity monitoring.

## Installation

```bash
npm install sctlin
```

**Peer dependency:** Svelte 5 (`svelte ^5.0.0`)

### Tailwind CSS

sctlin uses Tailwind CSS 4. Add the source path so Tailwind scans sctlin's components:

```css
/* app.css */
@import 'tailwindcss';
@source '../node_modules/sctlin/dist';
```

Without the `@source` directive, sctlin components render unstyled.

## Quick Start

### Widget (simplest — handles everything)

```svelte
<script>
  import { TerminalWidget } from 'sctlin/widgets';
</script>

<TerminalWidget config={{ wsUrl: 'ws://device:1337/api/ws', apiKey: 'your-key' }} />
```

### Components (full control)

```svelte
<script>
  import { TerminalContainer } from 'sctlin/components';
</script>

<TerminalContainer
  config={{
    wsUrl: 'ws://device:1337/api/ws',
    apiKey: 'your-key',
    autoConnect: true,
    autoStartSession: true
  }}
/>
```

## Import Paths

| Path | Contents |
|------|----------|
| `sctlin` | Everything (components, widgets, types, utils) |
| `sctlin/components` | UI components |
| `sctlin/widgets` | Self-contained widgets with built-in connection logic |
| `sctlin/types` | TypeScript type exports |
| `sctlin/utils` | `SctlWsClient`, `SctlRestClient`, `ConnectionManager`, `TransferTracker`, error classes |

## Architecture Overview

```
ConnectionManager
  ├── SctlWsClient    (WebSocket: sessions, real-time output, events)
  ├── SctlRestClient   (HTTP: device info, files, exec, playbooks, STP transfers)
  └── TransferTracker  (chunked file transfers via STP protocol)
        │
        ▼
TerminalContainer      (session UI: tabs, split panes, xterm.js instances)
Widgets                (self-contained: each creates its own clients internally)
```

**Callback-driven pattern**: sctlin uses no framework-specific stores or context. All state flows through plain callbacks (`SctlinCallbacks`). Wrap these callbacks in your framework's reactivity system (Svelte `$state`, React `useState`, Vue `ref`, etc.).

## Components

| Component | Description |
|-----------|-------------|
| `TerminalContainer` | Full terminal UI with tabs, control bar, session management |
| `Terminal` | Single xterm.js terminal instance |
| `TerminalTabs` | Tab bar for multiple sessions |
| `ControlBar` | Connection status, session controls |
| `ServerPanel` | Multi-server sidebar with session list |
| `DeviceInfoPanel` | Device info display (hostname, CPU, memory, disk) |
| `FileBrowser` | Remote file browser with preview |
| `ActivityFeed` | Compact activity log (sidebar) |
| `HistoryViewer` | Full activity history with filtering, search, pagination |
| `PlaybookList` | Playbook browser with select/delete actions |
| `PlaybookViewer` | Playbook detail: metadata, params, script |
| `PlaybookExecutor` | Playbook execution: param form, live preview, output |
| `CommandPalette` | Keyboard-driven command palette |
| `SearchBar` | Terminal text search |
| `QuickExecBar` | One-shot command execution |
| `SplitPane` | Resizable split pane layout |
| `ToastContainer` | Toast notifications |

## Widgets

Widgets bundle connection logic + UI. Pass a simple `DeviceConnectionConfig` instead of managing clients:

```ts
interface DeviceConnectionConfig {
  wsUrl: string;       // WebSocket URL (e.g. 'ws://device:1337/api/ws')
  apiKey: string;      // API key (Bearer token)
  autoConnect?: boolean; // default: true
}
```

| Widget | Description |
|--------|-------------|
| `TerminalWidget` | Self-contained terminal with auto-session |
| `DeviceStatusWidget` | Device info with periodic polling |
| `ActivityWidget` | Activity feed with real-time WebSocket updates |
| `PlaybookWidget` | Playbook browser + viewer + executor |

## ConnectionManager

`ConnectionManager` orchestrates multi-server connections. It creates and manages `SctlWsClient`, `SctlRestClient`, and `TransferTracker` instances per server.

### Basic Usage

```ts
import { ConnectionManager } from 'sctlin/utils';

const manager = new ConnectionManager(
  { maxActivityEntries: 200 },
  {
    onConnectionChange: (serverId, status) => { /* update UI */ },
    onDeviceInfo: (serverId, info) => { /* update device panel */ },
    onActivity: (serverId, entries) => { /* update activity feed */ },
  }
);

// Connect to a server
const conn = manager.connect({
  id: 'my-device',
  name: 'My Device',
  wsUrl: 'ws://device:1337/api/ws',
  apiKey: 'your-key',
  shell: ''
});

// Build config for TerminalContainer
const sctlinConfig = manager.buildSctlinConfig(serverConfig, {
  onSessionsChange: (sessions) => { /* update session list UI */ },
  onActiveSessionChange: (key) => { /* track active tab */ },
});
```

### Multi-Server Pattern

```ts
// Connect multiple devices
const conn1 = manager.connect(server1Config);
const conn2 = manager.connect(server2Config);

// Switch between them — build a new SctlinConfig for each
const config1 = manager.buildSctlinConfig(server1Config, callbacks);
const config2 = manager.buildSctlinConfig(server2Config, callbacks);

// Access connection state
const all = manager.getAll(); // ServerConnection[]
const one = manager.get('server-id'); // ServerConnection | undefined
```

### ConnectionManagerEvents

| Event | Parameters | Description |
|-------|-----------|-------------|
| `onConnectionChange` | `(serverId, status)` | WebSocket connection status changed |
| `onDeviceInfo` | `(serverId, info \| null)` | Device info fetched (null on failure) |
| `onActivity` | `(serverId, entries[])` | Full activity list updated |
| `onActivityNew` | `(serverId, entry)` | Single new activity entry arrived via WS |
| `onTransferChange` | `(serverId, transfers[])` | Transfer list changed (add/progress/complete) |
| `onTransferError` | `(serverId, transfer, message)` | Transfer failed |
| `onError` | `(serverId, error)` | Any error (connection, fetch, etc.) |

### Cleanup

```ts
manager.disconnect('server-id'); // Disconnect one server
manager.disconnectAll();          // Disconnect all
manager.destroy();                // Disconnect all + release resources
```

## SctlinConfig Reference

Configuration object passed to `TerminalContainer`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `wsUrl` | `string` | — | WebSocket URL (e.g. `'ws://host:1337/api/ws'`) |
| `apiKey` | `string` | — | API key for Bearer auth |
| `theme` | `TerminalTheme` | xterm defaults | Terminal colors and font |
| `defaultRows` | `number` | `24` | Default terminal rows |
| `defaultCols` | `number` | `80` | Default terminal columns |
| `autoConnect` | `boolean` | `true` | Connect WebSocket on mount |
| `autoStartSession` | `boolean` | `true` | Start a session once connected |
| `reconnect` | `Partial<ReconnectConfig>` | see below | WebSocket reconnect behavior |
| `callbacks` | `SctlinCallbacks` | — | State change callbacks |
| `sessionDefaults` | `Partial<SessionStartOptions>` | — | Defaults for new sessions |
| `client` | `SctlWsClient` | — | Pre-created WS client (skips internal creation) |

### SctlinCallbacks

| Callback | Parameters | Description |
|----------|-----------|-------------|
| `onConnectionChange` | `(status)` | WebSocket status changed |
| `onSessionStarted` | `(session)` | New session created |
| `onSessionClosed` | `(sessionId, reason)` | Session removed |
| `onSessionsChange` | `(sessions[])` | Session list changed (any session add/remove/update) |
| `onActiveSessionChange` | `(key \| null)` | Active tab changed |
| `onRemoteSessions` | `(sessions[])` | Server-side session list fetched |
| `onAiPermissionChange` | `(sessionId, allowed)` | AI permission toggled |
| `onAiStatusChange` | `(sessionId, working, activity?, message?)` | AI working status changed |
| `onSplitGroupsChange` | `(groups[])` | Split pane groups changed |
| `onFocusedPaneChange` | `(pane)` | Focused pane changed (`'primary'` or `'secondary'`) |
| `onResize` | `(sessionId, rows, cols)` | Terminal resized |
| `onError` | `(error)` | Error occurred (WsErrorMsg) |
| `onActivity` | `(entry)` | New activity entry from WS broadcast |

### SessionStartOptions

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `workingDir` | `string` | — | Initial working directory |
| `persistent` | `boolean` | `true` | Survive client disconnects |
| `env` | `Record<string, string>` | — | Environment variables |
| `shell` | `string` | device default | Shell binary path |
| `pty` | `boolean` | `true` | Enable PTY (terminal emulation) |
| `rows` | `number` | — | Initial terminal rows |
| `cols` | `number` | — | Initial terminal columns |
| `name` | `string` | — | Human-readable session name |

### ReconnectConfig

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `boolean` | `true` | Enable auto-reconnect |
| `initialDelay` | `number` | `100` | First retry delay in ms |
| `maxDelay` | `number` | `2000` | Maximum retry delay in ms |
| `maxAttempts` | `number` | `Infinity` | Max reconnect attempts |

## TerminalContainer Imperative API

`TerminalContainer` exports methods accessible via `bind:this`:

```svelte
<script>
  import { TerminalContainer } from 'sctlin/components';
  let terminal;
</script>

<TerminalContainer bind:this={terminal} config={...} />
<button onclick={() => terminal.startSession('/bin/bash')}>New Session</button>
```

### Session Lifecycle

| Method | Returns | Description |
|--------|---------|-------------|
| `startSession(shell?)` | `Promise<void>` | Start a new PTY session, opens a tab |
| `attachSession(sessionId)` | `Promise<void>` | Attach to a server session by its server ID |
| `listShells()` | `Promise<{shells, defaultShell}>` | Query available shells |
| `closeSession(key)` | `Promise<void>` | Close tab + kill server session |
| `killSessionById(sessionId)` | `Promise<void>` | Kill by server ID (works without local tab) |
| `detachSession(key)` | `void` | Close tab, leave server session running |
| `closeTab(key)` | `void` | Close tab without killing session |
| `renameSession(key, label)` | `void` | Rename session label (debounced 500ms) |

### Navigation

| Method | Returns | Description |
|--------|---------|-------------|
| `selectSession(key)` | `void` | Switch to tab by local key |
| `getActiveKey()` | `string \| null` | Currently active tab key |
| `getSessionList()` | `SessionInfo[]` | All session tabs |

### Split Panes

| Method | Returns | Description |
|--------|---------|-------------|
| `splitHorizontal()` | `void` | Split horizontally (or toggle/unsplit) |
| `splitVertical()` | `void` | Split vertically (or toggle/unsplit) |
| `unsplit()` | `void` | Remove split, keep focused pane |
| `toggleSplitFocus()` | `void` | Toggle focus between split panes |
| `getSplitPrimaryKey()` | `string \| null` | Primary pane key |
| `getSplitSecondaryKey()` | `string \| null` | Secondary pane key |
| `getSplitGroups()` | `SplitGroupInfo[]` | All split groups |

### UI

| Method | Returns | Description |
|--------|---------|-------------|
| `toggleSearch()` | `void` | Toggle terminal search bar |

### AI Control

| Method | Returns | Description |
|--------|---------|-------------|
| `setAllAi(allowed)` | `Promise<void>` | Set AI permission for all attached sessions |

### Execution

| Method | Returns | Description |
|--------|---------|-------------|
| `execInActiveSession(command)` | `void` | Send command to focused PTY session |
| `exec(command)` | `Promise<string>` | Run one-shot command (temp non-PTY session) |

### Remote

| Method | Returns | Description |
|--------|---------|-------------|
| `getRemoteSessions()` | `RemoteSessionInfo[]` | Latest server-side session list |
| `fetchRemoteSessions()` | `Promise<RemoteSessionInfo[]>` | Fetch fresh session list from server |

## Error Handling

sctlin uses a typed error hierarchy. All errors extend `SctlError`:

```
SctlError (base — has .code field)
├── ConnectionError    (code: 'connection_error')
├── ServerError        (code: server-provided, e.g. 'session_not_found')
├── TimeoutError       (code: 'timeout')
├── HttpError          (code: 'http_error', has .status and .body)
└── TransferError      (code: 'transfer_error', has .transferId)
```

### Catching Typed Errors

```ts
import { SctlError, TimeoutError, ServerError, HttpError } from 'sctlin/utils';

try {
  await ws.startSession();
} catch (e) {
  if (e instanceof TimeoutError) {
    console.log('Operation timed out');
  } else if (e instanceof ServerError) {
    console.log(`Server error: ${e.code} — ${e.message}`);
  } else if (e instanceof HttpError) {
    console.log(`HTTP ${e.status}: ${e.body}`);
  } else if (e instanceof SctlError) {
    console.log(`sctl error [${e.code}]: ${e.message}`);
  }
}
```

## Utilities

### SctlWsClient

WebSocket client with connection lifecycle, reconnect, and typed events:

```ts
import { SctlWsClient } from 'sctlin/utils';

const ws = new SctlWsClient('ws://device:1337/api/ws', 'your-key');
ws.connect();

ws.onStatusChange((status) => console.log(status));
const session = await ws.startSession({ pty: true, shell: '/bin/bash' });
ws.onOutput(session.session_id, (msg) => console.log(msg.data));
ws.sendStdin(session.session_id, 'ls\n');
```

### SctlRestClient

HTTP client for device APIs (derives base URL from WS URL):

```ts
import { SctlRestClient } from 'sctlin/utils';

const rest = new SctlRestClient('ws://device:1337/api/ws', 'your-key');
const info = await rest.getInfo();
const entries = await rest.listDir('/etc');
const result = await rest.exec('uname -a');
const playbooks = await rest.listPlaybooks();
```

### TransferTracker

Chunked file transfer engine with progress, SHA-256 verification, and retry:

```ts
import { TransferTracker } from 'sctlin/utils';

const tracker = new TransferTracker(restClient);
tracker.onprogress = (p) => console.log(`${p.filename}: ${Math.round(p.fraction * 100)}%`);
tracker.oncomplete = (t) => console.log(`Done: ${t.filename}`);

const blob = await tracker.download('/var/log/syslog');
await tracker.upload('/tmp', file);
```

## Integration Examples

### Minimal Terminal Embed

```svelte
<script>
  import { TerminalWidget } from 'sctlin/widgets';
</script>

<div class="h-screen">
  <TerminalWidget
    config={{ wsUrl: 'ws://device:1337/api/ws', apiKey: 'key' }}
  />
</div>
```

### Multi-Server Dashboard with ConnectionManager

```svelte
<script>
  import { ConnectionManager } from 'sctlin/utils';
  import { TerminalContainer } from 'sctlin/components';

  const servers = [
    { id: 'dev', name: 'Dev', wsUrl: 'ws://dev:1337/api/ws', apiKey: 'k1', shell: '' },
    { id: 'prod', name: 'Prod', wsUrl: 'ws://prod:1337/api/ws', apiKey: 'k2', shell: '' },
  ];

  let activeServer = $state(servers[0]);
  let statuses = $state({});

  const manager = new ConnectionManager({}, {
    onConnectionChange: (id, status) => { statuses[id] = status; statuses = statuses; },
  });

  for (const s of servers) manager.connect(s);

  let config = $derived(manager.buildSctlinConfig(activeServer, {
    onSessionsChange: (sessions) => { /* update sidebar */ },
  }));
</script>

<nav>
  {#each servers as s}
    <button onclick={() => activeServer = s}>{s.name} ({statuses[s.id] ?? '...'})</button>
  {/each}
</nav>
<TerminalContainer {config} />
```

### Monitoring-Only (No Terminal)

```svelte
<script>
  import { DeviceStatusWidget, ActivityWidget } from 'sctlin/widgets';
  const cfg = { wsUrl: 'ws://device:1337/api/ws', apiKey: 'key' };
</script>

<div class="grid grid-cols-2 gap-4">
  <DeviceStatusWidget config={cfg} pollInterval={15000} />
  <ActivityWidget config={cfg} maxEntries={50} />
</div>
```

## Development

```bash
npm install
npm run dev       # Start dev server
npm run check     # TypeScript check
npm run test      # Run tests
npm run package   # Build library (svelte-package + publint)
```

## License

GPL-3.0-only. See [LICENSE](../LICENSE).
