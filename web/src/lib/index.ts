// sctlin — Web terminal UI for sctl

// Components
export { default as TerminalContainer } from './components/TerminalContainer.svelte';
export { default as Terminal } from './components/Terminal.svelte';
export { default as TerminalTabs } from './components/TerminalTabs.svelte';
export { default as ControlBar } from './components/ControlBar.svelte';
export { default as ServerPanel } from './components/ServerPanel.svelte';
export { default as ToastContainer } from './components/ToastContainer.svelte';
export { default as SearchBar } from './components/SearchBar.svelte';
export { default as DeviceInfoPanel } from './components/DeviceInfoPanel.svelte';
export { default as QuickExecBar } from './components/QuickExecBar.svelte';
export { default as SplitPane } from './components/SplitPane.svelte';
export { default as FileBrowser } from './components/FileBrowser.svelte';
export { default as CommandPalette } from './components/CommandPalette.svelte';
export { default as ActivityFeed } from './components/ActivityFeed.svelte';
export { default as HistoryViewer } from './components/HistoryViewer.svelte';
export { default as PlaybookList } from './components/PlaybookList.svelte';
export { default as PlaybookViewer } from './components/PlaybookViewer.svelte';
export { default as PlaybookExecutor } from './components/PlaybookExecutor.svelte';
export { default as TransferIndicator } from './components/TransferIndicator.svelte';
export { default as PlaybookPanel } from './components/PlaybookPanel.svelte';
export { default as SidePanel } from './components/SidePanel.svelte';
export { default as ServerDashboard } from './components/ServerDashboard.svelte';
export { default as ExecViewer } from './components/ExecViewer.svelte';

// Widgets
export { default as DeviceStatusWidget } from './widgets/DeviceStatusWidget.svelte';
export { default as TerminalWidget } from './widgets/TerminalWidget.svelte';
export { default as ActivityWidget } from './widgets/ActivityWidget.svelte';
export { default as PlaybookWidget } from './widgets/PlaybookWidget.svelte';

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
	WsSessionRenameMsg,
	WsSessionAllowAiMsg,
	WsSessionRenameAckMsg,
	WsSessionAllowAiAckMsg,
	WsSessionCreatedBroadcast,
	WsSessionDestroyedBroadcast,
	WsSessionRenamedBroadcast,
	WsSessionAiPermissionChangedBroadcast,
	WsSessionAiStatusChangedBroadcast,
	WsSessionListMsg,
	RemoteSessionInfo,
	ServerConfig,
	DeviceInfo,
	NetworkInterface,
	DirEntry,
	FileContent,
	ExecResult,
	ActivityType,
	ActivitySource,
	ActivityEntry,
	WsActivityNewMsg,
	HistoryFilter,
	PlaybookParam,
	PlaybookSummary,
	PlaybookDetail,
	SplitGroupInfo,
	SidePanelTabDef,
	ViewerTab,
	ExecViewerData,
	FileViewerData,
	CachedExecResult
} from './types/terminal.types';

export type { DeviceConnectionConfig } from './types/widget.types';

// Utilities
export { SctlWsClient } from './utils/ws-client';
export { createTerminal, applyTheme, DEFAULT_THEME, type XtermInstance } from './utils/xterm';
export { SctlRestClient } from './utils/rest-client';
export { KeyboardManager, type Shortcut } from './utils/keyboard';
export { parsePlaybookFrontmatter, renderPlaybookScript, validatePlaybookName } from './utils/playbook-parser';
