// ── Side panel ──────────────────────────────────────────────────────

export interface SidePanelTabDef {
	id: string;
	label: string;
}

// ── Control & status enums ──────────────────────────────────────────

/** WebSocket connection lifecycle state. */
export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'reconnecting' | 'device_offline';

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

/** A single entry in the device activity log. */
export interface ActivityEntry {
	/** Monotonic server-assigned ID (increases with each operation). */
	id: number;
	/** Unix epoch seconds when the activity occurred. */
	timestamp: number;
	/** The type of operation (exec, file_read, session_start, etc.). */
	activity_type: ActivityType;
	/** Which client interface triggered this activity. */
	source: ActivitySource;
	/** Human-readable one-line description of the activity. */
	summary: string;
	/** Additional structured data (command args, file paths, exit codes, etc.). */
	detail?: Record<string, unknown>;
}

export interface WsActivityNewMsg {
	type: 'activity.new';
	entry: ActivityEntry;
}

// ── Theme ───────────────────────────────────────────────────────────

/**
 * xterm.js theme colors and font settings. All fields are optional —
 * unset fields use xterm defaults.
 */
export interface TerminalTheme {
	// UI colors
	background?: string;
	foreground?: string;
	cursor?: string;
	cursorAccent?: string;
	selectionBackground?: string;
	selectionForeground?: string;
	selectionInactiveBackground?: string;
	// ANSI standard colors (0–7)
	black?: string;
	red?: string;
	green?: string;
	yellow?: string;
	blue?: string;
	magenta?: string;
	cyan?: string;
	white?: string;
	// ANSI bright colors (8–15)
	brightBlack?: string;
	brightRed?: string;
	brightGreen?: string;
	brightYellow?: string;
	brightBlue?: string;
	brightMagenta?: string;
	brightCyan?: string;
	brightWhite?: string;
	// Font
	fontFamily?: string;
	fontSize?: number;
}

// ── Session ─────────────────────────────────────────────────────────

/** Options for starting a new shell session. */
export interface SessionStartOptions {
	/** Initial working directory (e.g. `'~'` or `'/tmp'`). */
	workingDir?: string;
	/** Whether the session survives client disconnects. Default: true. */
	persistent?: boolean;
	/** Environment variables to set in the session. */
	env?: Record<string, string>;
	/** Shell binary to use (e.g. `'/bin/bash'`). Uses device default if omitted. */
	shell?: string;
	/** Enable PTY (terminal emulation). Default: true. */
	pty?: boolean;
	/** Initial terminal rows. */
	rows?: number;
	/** Initial terminal columns. */
	cols?: number;
	/** Human-readable session name. */
	name?: string;
}

/**
 * Client-side session state tracked by TerminalContainer.
 *
 * `key` is a client-generated unique identifier (UUID) used for tab/pane management.
 * `sessionId` is the server-assigned session ID. Multiple keys can point to the same
 * sessionId (e.g. split panes), but each key has its own xterm instance.
 */
export interface SessionInfo {
	/** Client-generated unique key for this tab/pane (UUID). */
	key: string;
	/** Server-assigned session ID. */
	sessionId: string;
	pid?: number;
	persistent: boolean;
	pty: boolean;
	userAllowsAi: boolean;
	aiIsWorking: boolean;
	aiActivity?: string;
	aiStatusMessage?: string;
	/** Last output sequence number seen (for attach replay). */
	lastSeq: number;
	/** Human-readable label (from server name or rename). */
	label?: string;
	/** Whether this session's output is being received. */
	attached: boolean;
	/** Server ID this session belongs to (set in multi-server mode). */
	serverId?: string;
	/** Server display name (set when multiple servers connected). */
	serverName?: string;
	/** Session no longer exists on the server (e.g. device rebooted). */
	dead?: boolean;
}

// ── Reconnect ───────────────────────────────────────────────────────

/** WebSocket reconnection behavior. Pass as `Partial<ReconnectConfig>` to override defaults. */
export interface ReconnectConfig {
	/** Whether automatic reconnection is enabled. Default: true. */
	enabled: boolean;
	/** Delay in ms before the first retry (doubles each attempt). Default: 100. */
	initialDelay: number;
	/** Maximum delay in ms between retries. Default: 2000. */
	maxDelay: number;
	/** Maximum number of reconnect attempts before giving up. Default: Infinity. */
	maxAttempts: number;
}

// ── Split groups ────────────────────────────────────────────────────

export interface SplitGroupInfo {
	primaryKey: string;
	secondaryKey: string;
	direction: 'horizontal' | 'vertical';
}

// ── Callbacks ───────────────────────────────────────────────────────

/** Callbacks from TerminalContainer to the consumer for state synchronization. */
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

/**
 * Configuration for a TerminalContainer instance.
 *
 * Pass `client` to reuse a pre-created `SctlWsClient` (avoids duplicate connections).
 * Set `autoConnect: true` to connect immediately on mount.
 */
export interface SctlinConfig {
	/** WebSocket URL for the sctl device (e.g. `'ws://host:1337/api/ws'`). */
	wsUrl: string;
	/** API key for authentication (sent as Bearer token and WS query param). */
	apiKey: string;
	/** Terminal color/font theme applied to all xterm instances. */
	theme?: TerminalTheme;
	/** Default terminal rows for new sessions. */
	defaultRows?: number;
	/** Default terminal columns for new sessions. */
	defaultCols?: number;
	/** Connect to the WebSocket immediately on mount. Default: true. */
	autoConnect?: boolean;
	/** Automatically start a session once connected. Default: true. */
	autoStartSession?: boolean;
	/** WebSocket reconnection behavior overrides. */
	reconnect?: Partial<ReconnectConfig>;
	/** Callbacks for state synchronization with the consumer. */
	callbacks?: SctlinCallbacks;
	/** Default options applied to every new session (shell, env, workingDir, etc.). */
	sessionDefaults?: Partial<SessionStartOptions>;
	/** Pre-created WS client — skips client creation, reuses existing connection. */
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

export interface WsSessionGapMsg {
	type: 'session.gap';
	session_id: string;
	reason: string;
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

/** System information returned by the sctl device's `/api/info` endpoint. */
export interface DeviceInfo {
	/** Device serial identifier. */
	serial: string;
	/** System hostname. */
	hostname: string;
	/** Kernel version string (e.g. `'6.12.69'`). */
	kernel: string;
	/** System uptime in seconds. */
	system_uptime_secs: number;
	/** CPU model name. */
	cpu_model: string;
	/** 1/5/15-minute load averages. */
	load_average: [number, number, number];
	/** RAM usage in bytes. */
	memory: { total_bytes: number; used_bytes: number; available_bytes: number };
	/** Root filesystem usage in bytes. */
	disk: { total_bytes: number; used_bytes: number; available_bytes: number; path: string };
	/** Network interfaces with addresses and link state. */
	interfaces: NetworkInterface[];
	/** Tunnel relay connection info, if configured. */
	tunnel?: { connected: boolean; relay_url?: string; url?: string; reconnects?: number };
	/** GPS location data, if GPS is configured on the device. */
	gps?: {
		status: 'active' | 'searching' | 'error' | 'disabled';
		latitude?: number;
		longitude?: number;
		altitude?: number;
		satellites?: number;
		speed_kmh?: number;
		hdop?: number;
		fix_age_secs?: number;
	} | null;
	/** LTE signal quality, if LTE monitoring is configured on the device. */
	lte?: {
		rssi_dbm: number;
		rsrp?: number;
		rsrq?: number;
		sinr?: number;
		band?: string;
		operator?: string;
		technology?: string;
		cell_id?: string;
		signal_bars: number;
		pci?: number;
		earfcn?: number;
		freq_band?: number;
		tac?: string;
		plmn?: string;
		enodeb_id?: number;
		sector?: number;
		ul_bw_mhz?: string;
		dl_bw_mhz?: string;
		connection_state?: string;
		duplex?: string;
		neighbors?: NeighborCell[];
		band_config?: {
			enabled_bands: number[];
			priority_band?: number;
		};
		modem?: {
			model?: string;
			firmware?: string;
			imei?: string;
			iccid?: string;
		};
	} | null;
}

export interface NeighborCell {
	earfcn: number;
	pci: number;
	rsrp?: number;
	rsrq?: number;
	rssi?: number;
	sinr?: number;
	cell_type: string;
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

/** A recorded device connection session from the relay's connection history. */
export interface RelayConnectionSession {
	serial: string;
	connected_at: number;
	disconnected_at: number | null;
	duration_secs: number;
	reason: string | null;
	last_heartbeat_age_ms?: number | null;
}

export interface RelayLiveDevice {
	serial: string;
	connected: boolean;
	connected_at: number;
	connected_since_ms: number;
	last_heartbeat_age_ms: number;
	pending_requests_count: number;
	session_subscription_count: number;
	subscribed_client_count: number;
	client_count: number;
	dropped_messages: number;
	last_gps_fix: Record<string, unknown> | null;
	last_lte_signal: { rssi_dbm?: number; rsrp?: number; sinr?: number; signal_bars?: number; band?: string; operator?: string } | null;
}

/** Health response from a relay's /api/health endpoint. */
export interface RelayHealthInfo {
	status: string;
	uptime_secs: number;
	version: string;
	sessions: number;
	tunnel: {
		connected: boolean;
		reconnects: number;
		/** Enhanced fields (present when device runs tunnel client mode). */
		uptime_secs?: number;
		messages_sent?: number;
		messages_received?: number;
		last_pong_age_ms?: number;
		dropped_outbound?: number;
		stream_backpressure_events?: number;
		stream_replay_events?: number;
		rtt_median_ms?: number;
		rtt_p95_ms?: number;
		recent_events?: { time: string; event: string; detail?: string }[];
	};
	gps: { status: string; has_fix: boolean; fix_age_secs?: number; satellites?: number } | null;
	lte: { rssi_dbm?: number; rsrp?: number; sinr?: number; signal_bars?: number; band?: string; operator?: string; status?: string } | null;
	live_devices?: RelayLiveDevice[];
	connection_history?: RelayConnectionSession[];
	device_snapshots?: Record<string, DeviceSnapshot>;
}

/** Last-known device state snapshot from relay (survives disconnect + restart). */
export interface DeviceSnapshot {
	last_lte_signal: { rssi_dbm?: number; rsrp?: number; sinr?: number; signal_bars?: number; band?: string; operator?: string } | null;
	last_gps_fix: Record<string, unknown> | null;
	last_watchdog: { level?: number; action?: string; disconnect_secs?: number; signal_stale?: boolean; registration?: string } | null;
	last_seen: number;
}

/** A client-side event in the connection lifecycle log. */
export type ConnectionEventLevel = 'info' | 'warn' | 'error' | 'success';
export interface ConnectionEvent {
	/** Monotonic ID for keyed iteration. */
	id: number;
	/** Unix epoch ms. */
	timestamp: number;
	/** Severity level for color coding. */
	level: ConnectionEventLevel;
	/** Short summary line. */
	message: string;
	/** Optional detail (e.g. error code, duration). */
	detail?: string;
}

/** Result of probing a device through the relay's proxy endpoint. */
export interface DeviceProbeResult {
	/** Whether the device was reachable through the relay. */
	reachable: boolean;
	/** HTTP status code from the relay proxy, or null on network error. */
	status: number | null;
	/** Error code from the relay (e.g. 'DEVICE_NOT_FOUND', 'TIMEOUT'), or null if reachable. */
	errorCode: string | null;
	/** Human-readable error message, or null if reachable. */
	errorMessage: string | null;
	/** Timestamp of the probe. */
	probedAt: number;
}

/** Server-side diagnostics from `/api/diagnostics`. */
export interface ServerDiagnostics {
	process: {
		pid: number;
		rss_bytes: number;
		open_fds: number;
		threads: number;
		uptime_secs: number;
	};
	system: {
		hostname: string;
		os_uptime_secs: number;
		load_avg: number[];
		memory: { total_bytes: number; available_bytes: number; used_pct: number };
		disk: { path: string; total_bytes: number; available_bytes: number; used_bytes: number } | null;
	};
	network: {
		tcp: { established: number; listen: number; time_wait: number; close_wait: number };
	};
	logs: { timestamp: string; level: string; message: string }[];
	log_stats: { errors: number; warnings: number; total: number };
}

/** Configuration for a server connection (persisted in localStorage). */
export interface ServerConfig {
	/** Unique identifier for this server entry. */
	id: string;
	/** Human-readable display name. */
	name: string;
	/** WebSocket URL (e.g. `'ws://host:1337/api/ws'`). */
	wsUrl: string;
	/** API key for authentication (Bearer token). */
	apiKey: string;
	/** Preferred shell binary (empty string = device default). */
	shell: string;
	/** Optional API key for the relay server itself (for relay diagnostics). */
	relayApiKey?: string;
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
	| WsSessionGapMsg
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

/** A tab in the viewer panel (exec result or file content). */
export interface ViewerTab {
	key: string;
	type: 'exec' | 'file';
	label: string;
	icon: string;
	data: ExecViewerData | FileViewerData;
}

/** Data for an exec result viewer tab. */
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

/** Data for a file content viewer tab. */
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

// ── LTE band management types ──────────────────────────────────

/** A single signal observation on a specific band. */
export interface BandObservation {
	rsrp: number;
	rsrq?: number | null;
	sinr?: number | null;
	pci: number;
	recorded_at: number;
	serving: boolean;
}

/** Accumulated per-band signal history from passive monitoring. */
export interface BandHistoryEntry {
	band: number;
	best_rsrp: number;
	latest_rsrp: number;
	observation_count: number;
	last_seen: number;
	recent: BandObservation[];
}

/** Per-band result from a band scan. */
export interface ScanBandResult {
	band: number;
	registered: boolean;
	registration_time_ms: number;
	rsrp?: number | null;
	rsrq?: number | null;
	sinr?: number | null;
	download_bps?: number | null;
	upload_bps?: number | null;
}

/** Status of a running or completed band scan. */
export interface ScanStatus {
	state: 'running' | 'completed' | 'aborted';
	started_at: number;
	completed_at?: number | null;
	bands_to_scan: number[];
	bands_scanned: number[];
	current_band?: number | null;
	results: ScanBandResult[];
	original_bands: number[];
	original_priority?: number | null;
}

/** Full `/api/lte` response with signal, modem, band history, and scan status. */
export interface LteData {
	signal?: {
		rssi_dbm: number;
		rsrp?: number | null;
		rsrq?: number | null;
		sinr?: number | null;
		band?: string | null;
		operator?: string | null;
		technology?: string | null;
		cell_id?: string | null;
		pci?: number | null;
		earfcn?: number | null;
		freq_band?: number | null;
		tac?: string | null;
		plmn?: string | null;
		enodeb_id?: number | null;
		sector?: number | null;
		ul_bw_mhz?: string | null;
		dl_bw_mhz?: string | null;
		connection_state?: string | null;
		duplex?: string | null;
		neighbors?: NeighborCell[];
		band_config?: { enabled_bands: number[]; priority_band?: number | null };
		signal_bars?: number;
		recorded_at?: number;
	} | null;
	modem?: {
		model?: string | null;
		firmware?: string | null;
		imei?: string | null;
		iccid?: string | null;
	} | null;
	errors_total: number;
	last_error?: string | null;
	band_history: BandHistoryEntry[];
	scan_status?: ScanStatus | null;
	registration_pending?: boolean;
}

/** Request body for `POST /api/lte/bands`. */
export interface SetBandsRequest {
	mode: 'auto' | 'locked';
	bands?: number[];
	priority_band?: number;
	force?: boolean;
}

/** Response from `POST /api/lte/bands`. */
export interface SetBandsResult {
	status: string;
	mode: string;
	band_config: { enabled_bands: number[]; priority_band?: number | null };
	registration?: 'pending' | 'registered';
	error?: string;
}

/** Request body for `POST /api/lte/scan`. */
export interface StartScanRequest {
	bands?: number[];
	include_speed_test?: boolean;
	force?: boolean;
}

/** Response from `POST /api/lte/scan`. */
export interface StartScanResult {
	status: string;
	bands_to_scan: number[];
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
