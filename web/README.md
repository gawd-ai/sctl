# sctlin

Svelte 5 terminal UI component library for [sctl](../README.md), built on [xterm.js](https://xtermjs.org/).

## Overview

sctlin provides embeddable terminal components that connect to sctl's WebSocket API. It handles PTY sessions, terminal rendering, window resizing, and reconnection -- so you can drop a fully interactive remote terminal into any Svelte app.

## Components

| Component | Description |
|-----------|-------------|
| `TerminalContainer` | Full terminal UI with tabs, control bar, and session management |
| `Terminal` | Single xterm.js terminal instance |
| `TerminalTabs` | Tab bar for multiple sessions |
| `ControlBar` | Connection status, session controls |

## Installation

```bash
npm install sctlin
```

## Usage

```svelte
<script>
  import { TerminalContainer } from 'sctlin';
</script>

<TerminalContainer
  config={{
    serverUrl: 'ws://localhost:1337',
    apiKey: 'your-key'
  }}
/>
```

## Development

This package depends on `gawdux` (shared UI library) for the demo app. The core library components have no dependency on it.

```bash
npm install
npm run dev       # Start dev server
npm run package   # Build the library
```

## License

GPL-3.0-only. See [LICENSE](../LICENSE).
