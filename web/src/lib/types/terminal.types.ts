// ── Side panel ──────────────────────────────────────────────────────

export interface SidePanelTabDef {
	id: string;
	label: string;
}

// ── Control & status enums ──────────────────────────────────────────

/** WebSocket connection lifecycle state. */
export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'reconnecting' | 'device_offline';

// ── Activity feed ───────────────────────────────────────────────────
// Canonical definitions live in ./generated/ (driven by ts-rs from the
// Rust server crate). Re-exported here so existing imports continue to
// resolve without churn.

import type { ActivityType } from './generated/ActivityType';
import type { ActivitySource } from './generated/ActivitySource';
import type { ActivityEntry } from './generated/ActivityEntry';
export type { ActivityType, ActivitySource, ActivityEntry };

import type { WsServerMsg as GeneratedWsServerMsg } from './generated/WsServerMsg';
export type WsActivityNewMsg = Extract<GeneratedWsServerMsg, { type: 'activity.new' }>;

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

export interface WsJobStartMsg {
	type: 'job.start';
	request_id?: string;
	command: string;
	shell?: string;
	working_dir?: string;
	env?: Record<string, string>;
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
	| WsJobStartMsg
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

// All server → client message variants are derived from the generated
// `WsServerMsg` discriminated union (canonical source: Rust enum
// `crate::ws::messages::WsServerMsg`). Adding/removing a variant on the
// server propagates here on the next `cargo test export_bindings`.

export type WsPongMsg = Extract<GeneratedWsServerMsg, { type: 'pong' }>;
export type WsSessionStartedMsg = Extract<GeneratedWsServerMsg, { type: 'session.started' }>;
export type WsSessionExecAckMsg = Extract<GeneratedWsServerMsg, { type: 'session.exec.ack' }>;
export type WsSessionOutputMsg = Extract<
	GeneratedWsServerMsg,
	{ type: 'session.stdout' | 'session.stderr' | 'session.system' }
>;

/** Synthetic — not currently emitted by the server, kept for forward compatibility. */
export interface WsSessionGapMsg {
	type: 'session.gap';
	session_id: string;
	reason: string;
}

/**
 * Emitted by the server when a one-shot **job** process exits (see `job.start`).
 * Not sent for interactive terminal sessions. Declared here rather than derived
 * from the generated union because the device emits it as a raw frame.
 */
export interface WsSessionExitedMsg {
	type: 'session.exited';
	session_id: string;
	exit_code: number;
}

export type WsSessionClosedMsg = Extract<GeneratedWsServerMsg, { type: 'session.closed' }>;
export type WsSessionSignalAckMsg = Extract<GeneratedWsServerMsg, { type: 'session.signal.ack' }>;

/**
 * A single replayed buffer entry inside a `session.attached` payload. Each
 * entry is itself a full `session.stdout` / `session.stderr` / `session.system`
 * server message (with `type`, `session_id`, `data`, `seq`, `timestamp_ms`).
 */
export type WsSessionAttachEntry = Extract<
	GeneratedWsServerMsg,
	{ type: 'session.stdout' | 'session.stderr' | 'session.system' }
>;

/**
 * Refine the generated `session.attached` shape: entries are typed
 * `session.{stdout,stderr,system}` messages, not opaque `JsonValue`s.
 */
export type WsSessionAttachedMsg = Omit<
	Extract<GeneratedWsServerMsg, { type: 'session.attached' }>,
	'entries'
> & { entries: WsSessionAttachEntry[] };

export type WsSessionResizeAckMsg = Extract<GeneratedWsServerMsg, { type: 'session.resize.ack' }>;

// Canonical session-list payload (server-side struct: `SessionListItem`).
// Re-exported as `RemoteSessionInfo` to keep the historical TS name stable
// for the rest of the codebase.
import type { SessionListItem } from './generated/SessionListItem';
export type RemoteSessionInfo = SessionListItem;

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

export type WsSessionListedMsg = Extract<GeneratedWsServerMsg, { type: 'session.listed' }>;
export type WsShellListedMsg = Extract<GeneratedWsServerMsg, { type: 'shell.listed' }>;
export type WsErrorMsg = Extract<GeneratedWsServerMsg, { type: 'error' }>;
export type WsSessionRenameAckMsg = Extract<GeneratedWsServerMsg, { type: 'session.rename.ack' }>;
export type WsSessionCreatedBroadcast = Extract<GeneratedWsServerMsg, { type: 'session.created' }>;
export type WsSessionDestroyedBroadcast = Extract<GeneratedWsServerMsg, { type: 'session.destroyed' }>;
export type WsSessionRenamedBroadcast = Extract<GeneratedWsServerMsg, { type: 'session.renamed' }>;
export type WsSessionAllowAiAckMsg = Extract<GeneratedWsServerMsg, { type: 'session.allow_ai.ack' }>;
export type WsSessionAiPermissionChangedBroadcast = Extract<
	GeneratedWsServerMsg,
	{ type: 'session.ai_permission_changed' }
>;
export type WsSessionAiStatusChangedBroadcast = Extract<
	GeneratedWsServerMsg,
	{ type: 'session.ai_status_changed' }
>;

// Canonical server → client union sourced from the Rust enum via ts-rs.
// Synthetic variants (`session.gap`, `session.exited`) that don't exist on
// the server today are unioned in so consumers can still narrow on them
// defensively.
export type WsServerMsg = GeneratedWsServerMsg | WsSessionGapMsg | WsSessionExitedMsg;

// ── Transfer (gawdxfer / STP) types ────────────────────────────
// Canonical definitions in ./generated/ (driven by ts-rs from the
// Rust `gawdxfer::types` module). The Stp* aliases below match the
// historical naming scheme that's already wired into the rest of
// the codebase — they're 1:1 with the generated types underneath.

import type { Direction } from './generated/Direction';
import type { InitDownloadResult as GeneratedInitDownloadResult } from './generated/InitDownloadResult';
import type { InitUploadResult as GeneratedInitUploadResult } from './generated/InitUploadResult';
import type { ChunkAck as GeneratedChunkAck } from './generated/ChunkAck';
import type { ResumeResult as GeneratedResumeResult } from './generated/ResumeResult';
import type { StatusResult as GeneratedStatusResult } from './generated/StatusResult';
import type { TransferSummary as GeneratedTransferSummary } from './generated/TransferSummary';
import type { ListResult as GeneratedListResult } from './generated/ListResult';

export type TransferDirection = Direction;
export type StpInitDownloadResult = GeneratedInitDownloadResult;
export type StpInitUploadResult = GeneratedInitUploadResult;
export type StpChunkAck = GeneratedChunkAck;
export type StpResumeResult = GeneratedResumeResult;
export type StpStatusResult = GeneratedStatusResult;
export type StpTransferSummary = GeneratedTransferSummary;
export type StpListResult = GeneratedListResult;

export type GxProgressMsg = Extract<GeneratedWsServerMsg, { type: 'gx.progress' }>;
export type GxCompleteMsg = Extract<GeneratedWsServerMsg, { type: 'gx.complete' }>;

/** Synthetic — server emits `gx.complete` with `TransferError`-shaped data
 *  on failures rather than a separate `gx.error` variant. Kept for clients
 *  that wire failure handling onto a dedicated type. */
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
