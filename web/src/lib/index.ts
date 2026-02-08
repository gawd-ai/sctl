// sctlin — Web terminal UI for sctl

// Components
export { default as TerminalContainer } from './components/TerminalContainer.svelte';
export { default as Terminal } from './components/Terminal.svelte';
export { default as TerminalTabs } from './components/TerminalTabs.svelte';
export { default as ControlBar } from './components/ControlBar.svelte';
export { default as ServerPanel } from './components/ServerPanel.svelte';

// Types
export type {
	ConnectionStatus,
	TerminalTheme,
	SessionStartOptions,
	SessionInfo,
	ReconnectConfig,
	SctlinCallbacks,
	SctlinConfig,
	WsClientMsg,
	WsServerMsg,
	WsSessionStartedMsg,
	WsSessionOutputMsg,
	WsSessionClosedMsg,
	WsSessionExitedMsg,
	WsSessionAttachedMsg,
	WsSessionAttachEntry,
	WsErrorMsg,
	WsSessionResizeAckMsg,
	WsSessionResizeMsg,
	WsSessionExecAckMsg,
	WsSessionListedMsg,
	WsShellListMsg,
	WsShellListedMsg,
	RemoteSessionInfo,
	ServerConfig
} from './types/terminal.types';

// Utilities
export { SctlWsClient } from './utils/ws-client';
export { createTerminal, applyTheme, DEFAULT_THEME, type XtermInstance } from './utils/xterm';
