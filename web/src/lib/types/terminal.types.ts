// ── Side panel ──────────────────────────────────────────────────────

export interface SidePanelTabDef {
	id: string;
	label: string;
}

// ── Control & status enums ──────────────────────────────────────────

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'reconnecting';

// ── Activity feed ───────────────────────────────────────────────────

export type ActivityType =
	| 'exec'
	| 'file_read'
	| 'file_write'
	| 'file_list'
	| 'session_start'
	| 'session_exec'
	| 'session_kill'
	| 'session_signal'
	| 'playbook_list'
	| 'playbook_read'
	| 'playbook_write'
	| 'playbook_delete';

export type ActivitySource = 'mcp' | 'ws' | 'rest' | 'unknown';

export interface ActivityEntry {
	id: number;
	timestamp: number;
	activity_type: ActivityType;
	source: ActivitySource;
	summary: string;
	detail?: Record<string, unknown>;
}

export interface WsActivityNewMsg {
	type: 'activity.new';
	entry: ActivityEntry;
}

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

// ── Split groups ────────────────────────────────────────────────────

export interface SplitGroupInfo {
	primaryKey: string;
	secondaryKey: string;
	direction: 'horizontal' | 'vertical';
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
	onSplitGroupsChange?: (groups: SplitGroupInfo[]) => void;
	onFocusedPaneChange?: (pane: 'primary' | 'secondary') => void;
	onActivity?: (entry: ActivityEntry) => void;
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
	/** Pre-created WS client — skips client creation, starts immediately. */
	client?: import('../utils/ws-client').SctlWsClient;
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
	status?: 'running' | 'exited';
	idle?: boolean;
	idle_timeout?: number;
	exit_code?: number;
}

// ── REST API types ─────────────────────────────────────────────────

export interface DeviceInfo {
	serial: string;
	hostname: string;
	kernel: string;
	system_uptime_secs: number;
	cpu_model: string;
	load_average: [number, number, number];
	memory: { total_bytes: number; used_bytes: number; available_bytes: number };
	disk: { total_bytes: number; used_bytes: number; available_bytes: number; mount_point: string };
	interfaces: NetworkInterface[];
	tunnel?: { url: string; connected: boolean };
}

export interface NetworkInterface {
	name: string;
	state: string;
	mac: string;
	addresses: string[];
}

export interface DirEntry {
	name: string;
	type: 'file' | 'dir' | 'symlink' | 'other';
	size: number;
	mode?: string;
	modified?: string;
	symlink_target?: string;
}

export interface FileContent {
	content: string;
	encoding: string;
	size: number;
	path: string;
	truncated?: boolean;
}

export interface ExecResult {
	exit_code: number;
	stdout: string;
	stderr: string;
	duration_ms: number;
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
	| WsActivityNewMsg
	| GxProgressMsg
	| GxCompleteMsg
	| GxErrorMsg
	| WsErrorMsg;

// ── Transfer (gawdxfer / STP) types ────────────────────────────

export type TransferDirection = 'upload' | 'download';

export interface StpInitDownloadResult {
	transfer_id: string;
	file_size: number;
	file_hash: string;
	chunk_size: number;
	total_chunks: number;
	filename: string;
}

export interface StpInitUploadResult {
	transfer_id: string;
	chunk_size: number;
	total_chunks: number;
}

export interface StpChunkAck {
	transfer_id: string;
	chunk_index: number;
	ok: boolean;
	error?: string;
}

export interface StpResumeResult {
	transfer_id: string;
	direction: TransferDirection;
	chunks_received: number[];
	total_chunks: number;
	chunk_size: number;
	file_size: number;
	file_hash: string;
}

export interface StpStatusResult {
	transfer_id: string;
	direction: TransferDirection;
	phase: string;
	filename: string;
	file_size: number;
	chunks_done: number;
	total_chunks: number;
	bytes_transferred: number;
	elapsed_ms: number;
	error_count: number;
}

export interface StpTransferSummary {
	transfer_id: string;
	direction: TransferDirection;
	filename: string;
	file_size: number;
	phase: string;
	chunks_done: number;
	total_chunks: number;
	bytes_transferred: number;
}

export interface StpListResult {
	transfers: StpTransferSummary[];
}

export interface GxProgressMsg {
	type: 'gx.progress';
	data: {
		transfer_id: string;
		direction: TransferDirection;
		path: string;
		filename: string;
		chunks_done: number;
		total_chunks: number;
		bytes_transferred: number;
		file_size: number;
		elapsed_ms: number;
		rate_bps: number;
	};
}

export interface GxCompleteMsg {
	type: 'gx.complete';
	data: {
		transfer_id: string;
		direction: TransferDirection;
		path: string;
		filename: string;
		file_size: number;
		file_hash: string;
		elapsed_ms: number;
	};
}

export interface GxErrorMsg {
	type: 'gx.error';
	data: {
		transfer_id: string;
		code: string;
		message: string;
		recoverable: boolean;
	};
}

// ── Viewer tabs ────────────────────────────────────────────────

export interface ViewerTab {
	key: string;
	type: 'exec' | 'file';
	label: string;
	icon: string;
	data: ExecViewerData | FileViewerData;
}

export interface ExecViewerData {
	activityId: number;
	command: string;
	exitCode: number;
	stdout: string;
	stderr: string;
	durationMs: number;
	status: string;
	errorMessage?: string;
}

export interface FileViewerData {
	path: string;
	content: string;
	size: number;
}

export interface CachedExecResult {
	activity_id: number;
	exit_code: number;
	stdout: string;
	stderr: string;
	duration_ms: number;
	command: string;
	status: string;
	error_message?: string;
}

// ── History/Activity filtering ─────────────────────────────────

export interface HistoryFilter {
	activityTypes?: ActivityType[];
	sources?: ActivitySource[];
	search?: string;
}

// ── Playbook types ─────────────────────────────────────────────

export interface PlaybookParam {
	type: string;
	description: string;
	default?: unknown;
	enum?: unknown[];
}

export interface PlaybookSummary {
	name: string;
	description: string;
	params: string[];
}

export interface PlaybookDetail {
	name: string;
	description: string;
	params: Record<string, PlaybookParam>;
	script: string;
	raw_content: string;
}
