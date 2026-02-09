// ── Control & status enums ──────────────────────────────────────────

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'reconnecting';

// ── Theme ───────────────────────────────────────────────────────────

export interface TerminalTheme {
	background?: string;
	foreground?: string;
	cursor?: string;
	cursorAccent?: string;
	selectionBackground?: string;
	selectionForeground?: string;
	selectionInactiveBackground?: string;
	black?: string;
	red?: string;
	green?: string;
	yellow?: string;
	blue?: string;
	magenta?: string;
	cyan?: string;
	white?: string;
	brightBlack?: string;
	brightRed?: string;
	brightGreen?: string;
	brightYellow?: string;
	brightBlue?: string;
	brightMagenta?: string;
	brightCyan?: string;
	brightWhite?: string;
	fontFamily?: string;
	fontSize?: number;
}

// ── Session ─────────────────────────────────────────────────────────

export interface SessionStartOptions {
	workingDir?: string;
	persistent?: boolean;
	env?: Record<string, string>;
	shell?: string;
	pty?: boolean;
	rows?: number;
	cols?: number;
	name?: string;
}

export interface SessionInfo {
	key: string;
	sessionId: string;
	pid?: number;
	persistent: boolean;
	pty: boolean;
	userAllowsAi: boolean;
	aiIsWorking: boolean;
	aiActivity?: string;
	aiStatusMessage?: string;
	lastSeq: number;
	label?: string;
	attached: boolean;
	serverId?: string;
	serverName?: string;
	/** Session no longer exists on the server (e.g. device rebooted). */
	dead?: boolean;
}

// ── Reconnect ───────────────────────────────────────────────────────

export interface ReconnectConfig {
	enabled: boolean;
	initialDelay: number;
	maxDelay: number;
	maxAttempts: number;
}

// ── Callbacks ───────────────────────────────────────────────────────

export interface SctlinCallbacks {
	onConnectionChange?: (status: ConnectionStatus) => void;
	onSessionStarted?: (session: SessionInfo) => void;
	onSessionClosed?: (sessionId: string, reason: string) => void;
	onAiPermissionChange?: (sessionId: string, allowed: boolean) => void;
	onAiStatusChange?: (sessionId: string, working: boolean, activity?: string, message?: string) => void;
	onError?: (error: WsErrorMsg) => void;
	onResize?: (sessionId: string, rows: number, cols: number) => void;
	onRemoteSessions?: (sessions: RemoteSessionInfo[]) => void;
	onSessionsChange?: (sessions: SessionInfo[]) => void;
	onActiveSessionChange?: (sessionId: string | null) => void;
}

// ── Config ──────────────────────────────────────────────────────────

export interface SctlinConfig {
	wsUrl: string;
	apiKey: string;
	theme?: TerminalTheme;
	defaultRows?: number;
	defaultCols?: number;
	autoConnect?: boolean;
	autoStartSession?: boolean;
	reconnect?: Partial<ReconnectConfig>;
	callbacks?: SctlinCallbacks;
	sessionDefaults?: Partial<SessionStartOptions>;
}

// ── Wire protocol: client → server ─────────────────────────────────

export interface WsPingMsg {
	type: 'ping';
	request_id?: string;
}

export interface WsSessionStartMsg {
	type: 'session.start';
	request_id?: string;
	working_dir?: string;
	persistent?: boolean;
	env?: Record<string, string>;
	shell?: string;
	pty?: boolean;
	rows?: number;
	cols?: number;
	name?: string;
}

export interface WsSessionExecMsg {
	type: 'session.exec';
	request_id?: string;
	session_id: string;
	command: string;
}

export interface WsSessionStdinMsg {
	type: 'session.stdin';
	session_id: string;
	data: string;
}

export interface WsSessionKillMsg {
	type: 'session.kill';
	request_id?: string;
	session_id: string;
}

export interface WsSessionSignalMsg {
	type: 'session.signal';
	request_id?: string;
	session_id: string;
	signal: number;
}

export interface WsSessionAttachMsg {
	type: 'session.attach';
	request_id?: string;
	session_id: string;
	since?: number;
}

export interface WsSessionResizeMsg {
	type: 'session.resize';
	request_id?: string;
	session_id: string;
	rows: number;
	cols: number;
}

export interface WsSessionListMsg {
	type: 'session.list';
	request_id?: string;
}

export interface WsShellListMsg {
	type: 'shell.list';
	request_id?: string;
}

export interface WsSessionRenameMsg {
	type: 'session.rename';
	request_id?: string;
	session_id: string;
	name: string;
}

export interface WsSessionAllowAiMsg {
	type: 'session.allow_ai';
	request_id?: string;
	session_id: string;
	allowed: boolean;
}

export type WsClientMsg =
	| WsPingMsg
	| WsSessionStartMsg
	| WsSessionExecMsg
	| WsSessionStdinMsg
	| WsSessionKillMsg
	| WsSessionSignalMsg
	| WsSessionAttachMsg
	| WsSessionResizeMsg
	| WsSessionListMsg
	| WsShellListMsg
	| WsSessionRenameMsg
	| WsSessionAllowAiMsg;

// ── Wire protocol: server → client ─────────────────────────────────

export interface WsPongMsg {
	type: 'pong';
	request_id?: string;
}

export interface WsSessionStartedMsg {
	type: 'session.started';
	request_id?: string;
	session_id: string;
	pid: number;
	pty: boolean;
}

export interface WsSessionExecAckMsg {
	type: 'session.exec.ack';
	request_id?: string;
	session_id: string;
}

export interface WsSessionOutputMsg {
	type: 'session.stdout' | 'session.stderr' | 'session.system';
	session_id: string;
	data: string;
	seq: number;
	timestamp_ms?: number;
}

export interface WsSessionExitedMsg {
	type: 'session.exited';
	session_id: string;
	exit_code: number;
}

export interface WsSessionClosedMsg {
	type: 'session.closed';
	request_id?: string;
	session_id: string;
	reason: string;
}

export interface WsSessionSignalAckMsg {
	type: 'session.signal.ack';
	request_id?: string;
	session_id: string;
}

export interface WsSessionAttachEntry {
	stream: 'stdout' | 'stderr' | 'system';
	data: string;
	seq: number;
	timestamp_ms: number;
}

export interface WsSessionAttachedMsg {
	type: 'session.attached';
	request_id?: string;
	session_id: string;
	entries: WsSessionAttachEntry[];
}

export interface WsSessionResizeAckMsg {
	type: 'session.resize.ack';
	request_id?: string;
	session_id: string;
	rows: number;
	cols: number;
}

export interface RemoteSessionInfo {
	session_id: string;
	pid: number;
	persistent: boolean;
	pty: boolean;
	attached: boolean;
	name?: string;
	user_allows_ai?: boolean;
	ai_is_working?: boolean;
	ai_activity?: string;
	ai_status_message?: string;
}

export interface ServerConfig {
	id: string;
	name: string;
	wsUrl: string;
	apiKey: string;
	shell: string;
}

export interface WsSessionListedMsg {
	type: 'session.listed';
	request_id?: string;
	sessions: RemoteSessionInfo[];
}

export interface WsShellListedMsg {
	type: 'shell.listed';
	request_id?: string;
	shells: string[];
	default_shell: string;
}

export interface WsErrorMsg {
	type: 'error';
	request_id?: string;
	code: string;
	message: string;
	session_id?: string;
}

export interface WsSessionRenameAckMsg {
	type: 'session.rename.ack';
	request_id?: string;
	session_id: string;
	name: string;
}

export interface WsSessionCreatedBroadcast {
	type: 'session.created';
	session_id: string;
	pid: number;
	pty: boolean;
	persistent: boolean;
	name?: string;
}

export interface WsSessionDestroyedBroadcast {
	type: 'session.destroyed';
	session_id: string;
	reason: string;
}

export interface WsSessionRenamedBroadcast {
	type: 'session.renamed';
	session_id: string;
	name: string;
}

export interface WsSessionAllowAiAckMsg {
	type: 'session.allow_ai.ack';
	request_id?: string;
	session_id: string;
	allowed: boolean;
}

export interface WsSessionAiPermissionChangedBroadcast {
	type: 'session.ai_permission_changed';
	session_id: string;
	allowed: boolean;
}

export interface WsSessionAiStatusChangedBroadcast {
	type: 'session.ai_status_changed';
	session_id: string;
	working: boolean;
	activity?: string;
	message?: string;
}

export type WsServerMsg =
	| WsPongMsg
	| WsSessionStartedMsg
	| WsSessionExecAckMsg
	| WsSessionOutputMsg
	| WsSessionExitedMsg
	| WsSessionClosedMsg
	| WsSessionSignalAckMsg
	| WsSessionAttachedMsg
	| WsSessionResizeAckMsg
	| WsSessionRenameAckMsg
	| WsSessionListedMsg
	| WsShellListedMsg
	| WsSessionCreatedBroadcast
	| WsSessionDestroyedBroadcast
	| WsSessionRenamedBroadcast
	| WsSessionAllowAiAckMsg
	| WsSessionAiPermissionChangedBroadcast
	| WsSessionAiStatusChangedBroadcast
	| WsErrorMsg;
