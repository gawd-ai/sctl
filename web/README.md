# sctlin

Svelte 5 component library for [sctl](../README.md) -- embeddable terminal, device management, playbook execution, and activity monitoring.

## Installation

```bash
npm install sctlin
```

**Peer dependency:** Svelte 5 (`svelte ^5.0.0`)

## Quick Start

### Widget (simplest -- handles everything)

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
| `sctlin/utils` | `SctlWsClient`, `SctlRestClient`, helpers |

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

Widgets bundle connection logic + UI. Pass a simple config instead of managing clients:

```ts
interface DeviceConnectionConfig {
  wsUrl: string;
  apiKey: string;
  autoConnect?: boolean; // default: true
}
```

| Widget | Description |
|--------|-------------|
| `TerminalWidget` | Self-contained terminal with auto-session |
| `DeviceStatusWidget` | Device info with polling |
| `ActivityWidget` | Activity feed with real-time updates |
| `PlaybookWidget` | Playbook browser + viewer + executor |

## Tailwind CSS

sctlin uses Tailwind CSS 4. To include sctlin's styles in your Tailwind build, add the source path:

```css
/* app.css */
@import 'tailwindcss';
@source '../node_modules/sctlin/dist';
```

## Utilities

```ts
import { SctlWsClient, SctlRestClient } from 'sctlin/utils';

// REST client
const rest = new SctlRestClient('ws://device:1337/api/ws', 'your-key');
const info = await rest.getInfo();
const playbooks = await rest.listPlaybooks();

// WebSocket client
const ws = new SctlWsClient('ws://device:1337/api/ws', 'your-key');
ws.connect();
const session = await ws.startSession({ pty: true });
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
