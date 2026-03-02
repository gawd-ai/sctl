/**
 * Framework-agnostic connection orchestrator for sctl devices.
 *
 * Encapsulates the lifecycle of multi-server connections: WS client creation,
 * REST client creation, transfer tracker wiring, device info fetching, and
 * activity feed polling. Consumers receive state updates via plain callbacks
 * and can wrap them in any reactivity system (Svelte `$state`, React `useState`, etc.).
 *
 * Does NOT manage sessions (that stays in `TerminalContainer`) or UI state
 * (active server, panel open/close, viewer tabs — those stay in the consumer).
 *
 * @example
 * ```ts
 * const manager = new ConnectionManager(
 *   { maxActivityEntries: 200 },
 *   {
 *     onConnectionChange: (id, status) => { ... },
 *     onDeviceInfo: (id, info) => { ... },
 *     onActivity: (id, entries) => { ... },
 *   }
 * );
 *
 * const conn = manager.connect(serverConfig);
 * const sctlinCfg = manager.buildSctlinConfig(serverConfig, { onSessionsChange: ... });
 * // ... pass sctlinCfg to TerminalContainer
 *
 * manager.disconnect(serverConfig.id);
 * manager.destroy(); // cleanup all connections
 * ```
 */

import { SctlWsClient, type WsClientConfig } from './ws-client';
import { SctlRestClient, type RestClientConfig } from './rest-client';
import { TransferTracker, type ClientTransfer } from './transfer';
import { getRelayBaseUrl, getRelaySerial } from './relay';
import type {
	ConnectionStatus,
	ReconnectConfig,
	SessionStartOptions,
	ServerConfig,
	DeviceInfo,
	ActivityEntry,
	RelayHealthInfo,
	DeviceProbeResult,
	ConnectionEvent,
	ConnectionEventLevel,
	SctlinConfig,
	SctlinCallbacks,
	ServerDiagnostics
} from '../types/terminal.types';

/** Configuration for the ConnectionManager. */
export interface ConnectionManagerConfig {
	/** Timeout in ms for REST API requests. Passed to `RestClientConfig.timeout`. */
	httpTimeout?: number;
	/** Interval in ms between WebSocket keepalive pings. Passed to `WsClientConfig.pingInterval`. */
	pingInterval?: number;
	/** Timeout in ms to wait for WebSocket ack responses. Passed to `WsClientConfig.ackTimeout`. */
	ackTimeout?: number;
	/** Timeout in ms for STP chunk transfers. Passed to `RestClientConfig.chunkTimeout`. */
	chunkTimeout?: number;
	/** Maximum number of activity entries to retain per server. Default: 200. */
	maxActivityEntries?: number;
	/** Fetch device info automatically on connect. Default: true. */
	autoFetchInfo?: boolean;
	/** Fetch activity log automatically on connect. Default: true. */
	autoFetchActivity?: boolean;
	/** WebSocket reconnect configuration. */
	reconnect?: Partial<ReconnectConfig>;
	/** Default session options applied when building SctlinConfig. */
	sessionDefaults?: Partial<SessionStartOptions>;
}

/** Represents a live connection to a single sctl server. */
export interface ServerConnection {
	/** The server ID (from `ServerConfig.id`). */
	readonly id: string;
	/** The server config this connection was created from. */
	readonly config: ServerConfig;
	/** The WebSocket client for this connection. */
	readonly wsClient: SctlWsClient;
	/** The REST client for this connection. */
	readonly restClient: SctlRestClient;
	/** The file transfer tracker for this connection. */
	readonly transferTracker: TransferTracker;
	/** Current connection status. */
	status: ConnectionStatus;
	/** Device info fetched after connect, or null if not yet available. */
	deviceInfo: DeviceInfo | null;
	/** Activity log entries for this server. */
	activity: ActivityEntry[];
	/** Whether this connection goes through a relay (detected from wsUrl). */
	readonly isRelay: boolean;
	/** HTTP base URL of the relay, if this is a relay connection. */
	readonly relayBaseUrl: string | null;
	/** Device serial extracted from the relay URL, or null for direct connections. */
	readonly relaySerial: string | null;
	/** Relay health info fetched from /api/health, or null if not yet available. */
	relayHealth: RelayHealthInfo | null;
	/** Relay system info fetched from /api/info (requires relayApiKey), or null. */
	relayInfo: DeviceInfo | null;
	/** API key for the relay server itself, if configured. */
	readonly relayApiKey: string | null;
	/** Reason the device disconnected from the relay (from tunnel.device_disconnected). */
	disconnectReason: string | null;
	/** Timestamp when the connection last reached 'connected' status. */
	lastConnectedAt: number | null;
	/** Latest device probe result (from probing relay's /d/{serial}/api/health). */
	deviceProbe: DeviceProbeResult | null;
	/** Client-side connection lifecycle event log. */
	connectionLog: ConnectionEvent[];
}

/** Callbacks for ConnectionManager state changes. */
export interface ConnectionManagerEvents {
	/** Fired when a server's WebSocket connection status changes. */
	onConnectionChange?: (serverId: string, status: ConnectionStatus) => void;
	/** Fired when device info is fetched (or null on failure). */
	onDeviceInfo?: (serverId: string, info: DeviceInfo | null) => void;
	/** Fired when the full activity list is updated (initial fetch or capped). */
	onActivity?: (serverId: string, entries: ActivityEntry[]) => void;
	/** Fired when a single new activity entry arrives via WebSocket. */
	onActivityNew?: (serverId: string, entry: ActivityEntry) => void;
	/** Fired when the transfer list changes for a server. */
	onTransferChange?: (serverId: string, transfers: ClientTransfer[]) => void;
	/** Fired when a transfer encounters an error. */
	onTransferError?: (serverId: string, transfer: ClientTransfer, message: string) => void;
	/** Fired when relay health is fetched (or null on failure). */
	onRelayHealth?: (serverId: string, health: RelayHealthInfo | null) => void;
	/** Fired when relay system info is fetched (or null on failure). */
	onRelayInfo?: (serverId: string, info: DeviceInfo | null) => void;
	/** Fired when a device disconnect reason is received from the relay. */
	onDisconnectReason?: (serverId: string, reason: string) => void;
	/** Fired when a device probe completes. */
	onDeviceProbe?: (serverId: string, result: DeviceProbeResult) => void;
	/** Fired when a connection log event is added. */
	onConnectionLog?: (serverId: string, events: ConnectionEvent[]) => void;
	/** Fired on any error (connection, fetch, etc.). */
	onError?: (serverId: string, error: Error) => void;
}

export class ConnectionManager {
	private connections = new Map<string, ServerConnection>();
	private unsubscribers = new Map<string, (() => void)[]>();
	private readonly config: Required<Omit<ConnectionManagerConfig, 'reconnect' | 'sessionDefaults'>>;
	private readonly reconnectConfig?: Partial<ReconnectConfig>;
	private readonly sessionDefaults?: Partial<SessionStartOptions>;
	private readonly events: ConnectionManagerEvents;
	private eventIdCounter = 0;
	/** Fingerprint of last relay health per server, for dedup. */
	private relayHealthFingerprints = new Map<string, string>();
	/** Log entry IDs from the last relay health check per server. */
	private relayHealthLogIds = new Map<string, number[]>();

	constructor(config?: ConnectionManagerConfig, events?: ConnectionManagerEvents) {
		this.config = {
			httpTimeout: config?.httpTimeout ?? 30_000,
			pingInterval: config?.pingInterval ?? 30_000,
			ackTimeout: config?.ackTimeout ?? 10_000,
			chunkTimeout: config?.chunkTimeout ?? 60_000,
			maxActivityEntries: config?.maxActivityEntries ?? 200,
			autoFetchInfo: config?.autoFetchInfo ?? true,
			autoFetchActivity: config?.autoFetchActivity ?? true,
		};
		this.reconnectConfig = config?.reconnect;
		this.sessionDefaults = config?.sessionDefaults;
		this.events = events ?? {};
	}

	/** Format a duration in ms to a human-readable string. */
	private formatDuration(ms: number): string {
		const secs = Math.floor(ms / 1000);
		if (secs < 60) return `${secs}s`;
		if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
		return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
	}

	/** Push an event to a connection's log and notify listeners. */
	private logEvent(serverId: string, level: ConnectionEventLevel, message: string, detail?: string): void {
		const conn = this.connections.get(serverId);
		if (!conn) return;
		const event: ConnectionEvent = {
			id: ++this.eventIdCounter,
			timestamp: Date.now(),
			level,
			message,
			detail,
		};
		conn.connectionLog = [...conn.connectionLog, event].slice(-200);
		this.events.onConnectionLog?.(serverId, conn.connectionLog);
	}

	/** Build a fingerprint of relay health for dedup (excludes uptime which always changes). */
	private relayHealthFingerprint(h: RelayHealthInfo): string {
		const parts: (string | number | boolean | null | undefined)[] = [
			h.status, h.sessions, h.tunnel.connected, h.tunnel.reconnects,
		];
		if (h.lte) {
			parts.push(h.lte.signal_bars, h.lte.rsrp, h.lte.sinr, h.lte.band, h.lte.operator);
		}
		if (h.gps) {
			parts.push(h.gps.status, h.gps.satellites, h.gps.has_fix);
		}
		return parts.map(String).join('|');
	}

	/**
	 * Connect to a server. Creates WS client, REST client, and transfer tracker,
	 * starts the WebSocket connection, and optionally fetches device info and activity.
	 *
	 * If already connected to this server ID, returns the existing connection.
	 */
	connect(server: ServerConfig): ServerConnection {
		const existing = this.connections.get(server.id);
		if (existing) return existing;

		const wsConfig: WsClientConfig = {
			pingInterval: this.config.pingInterval,
			ackTimeout: this.config.ackTimeout,
		};
		const restConfig: RestClientConfig = {
			timeout: this.config.httpTimeout,
			chunkTimeout: this.config.chunkTimeout,
		};

		const wsClient = new SctlWsClient(server.wsUrl, server.apiKey, this.reconnectConfig, wsConfig);
		const restClient = new SctlRestClient(server.wsUrl, server.apiKey, restConfig);
		const transferTracker = new TransferTracker(restClient);

		const relayBase = getRelayBaseUrl(server.wsUrl);
		const relaySerial = getRelaySerial(server.wsUrl);
		const conn: ServerConnection = {
			id: server.id,
			config: server,
			wsClient,
			restClient,
			transferTracker,
			status: 'connecting',
			deviceInfo: null,
			activity: [],
			isRelay: relayBase !== null,
			relayBaseUrl: relayBase,
			relaySerial,
			relayApiKey: server.relayApiKey || null,
			relayHealth: null,
			relayInfo: null,
			disconnectReason: null,
			lastConnectedAt: null,
			deviceProbe: null,
			connectionLog: [],
		};

		// Wire transfer tracker events
		transferTracker.onchange = () => {
			this.events.onTransferChange?.(server.id, transferTracker.activeTransfers);
		};
		transferTracker.onerror = (ct, msg) => {
			this.events.onTransferError?.(server.id, ct, msg);
		};

		// Wire WS status changes
		const unsubs: (() => void)[] = [];

		unsubs.push(wsClient.onStatusChange((status) => {
			const prevStatus = conn.status;
			conn.status = status;
			this.events.onConnectionChange?.(server.id, status);

			// Log status transitions
			if (status === 'connected') {
				const duration = conn.lastConnectedAt
					? ` (offline ${this.formatDuration(Date.now() - conn.lastConnectedAt)})`
					: '';
				conn.lastConnectedAt = Date.now();
				conn.disconnectReason = null;
				conn.deviceProbe = null;
				if (prevStatus === 'reconnecting' || prevStatus === 'device_offline') {
					this.logEvent(server.id, 'success', `reconnected${duration}`);
				} else {
					this.logEvent(server.id, 'success', 'connected',
						conn.isRelay ? `via relay (${conn.relaySerial})` : undefined);
				}
				if (this.config.autoFetchInfo) {
					this.fetchDeviceInfo(server.id).catch(() => {});
				}
				if (this.config.autoFetchActivity) {
					this.fetchActivity(server.id).catch(() => {});
				}
				if (conn.isRelay) {
					this.fetchRelayHealth(server.id).catch(() => {});
					if (conn.relayApiKey) {
						this.fetchRelayInfo(server.id).catch(() => {});
					}
				}
			} else if (status === 'device_offline') {
				this.logEvent(server.id, 'warn', 'device offline',
					conn.disconnectReason ? `reason: ${conn.disconnectReason}` : 'relay reachable, device not connected');
				if (conn.isRelay) {
					this.fetchRelayHealth(server.id).catch(() => {});
					this.probeRelayDevice(server.id).catch(() => {});
					if (conn.relayApiKey && !conn.relayInfo) {
						this.fetchRelayInfo(server.id).catch(() => {});
					}
				}
			} else if (status === 'reconnecting') {
				this.logEvent(server.id, 'warn', 'connection lost, reconnecting...');
			} else if (status === 'disconnected' && prevStatus !== 'disconnected') {
				this.logEvent(server.id, 'info', 'disconnected');
			} else if (status === 'connecting' && prevStatus === 'disconnected') {
				this.logEvent(server.id, 'info', 'connecting...',
					conn.isRelay ? `relay → ${conn.relaySerial}` : server.wsUrl);
			}
		}));

		// Capture tunnel.device_disconnected reason from relay
		if (conn.isRelay) {
			unsubs.push(wsClient.on('tunnel.device_disconnected' as 'error', (msg: unknown) => {
				const m = msg as { reason?: string };
				const reason = m.reason ?? 'unknown';
				conn.disconnectReason = reason;
				this.events.onDisconnectReason?.(server.id, reason);
				this.logEvent(server.id, 'error', `device disconnected: ${reason}`);
			}));
		}

		this.connections.set(server.id, conn);
		this.unsubscribers.set(server.id, unsubs);

		this.logEvent(server.id, 'info', 'connecting...',
			conn.isRelay ? `relay → ${conn.relaySerial}` : server.wsUrl);

		wsClient.connect();
		return conn;
	}

	/**
	 * Disconnect from a server. Closes the WebSocket, cleans up listeners,
	 * and removes the connection from the manager.
	 */
	disconnect(serverId: string): void {
		const conn = this.connections.get(serverId);
		if (!conn) return;

		// Unsubscribe all listeners
		const unsubs = this.unsubscribers.get(serverId);
		if (unsubs) {
			for (const u of unsubs) u();
			this.unsubscribers.delete(serverId);
		}

		conn.wsClient.disconnect();
		this.connections.delete(serverId);
		this.relayHealthFingerprints.delete(serverId);
		this.relayHealthLogIds.delete(serverId);
	}

	/** Get a connection by server ID, or undefined if not connected. */
	get(serverId: string): ServerConnection | undefined {
		return this.connections.get(serverId);
	}

	/** Get all active connections. */
	getAll(): ServerConnection[] {
		return [...this.connections.values()];
	}

	/**
	 * Build an `SctlinConfig` for a `TerminalContainer`, wiring connection events
	 * back through this manager. The consumer can pass additional `SctlinCallbacks`
	 * that are merged (consumer callbacks are called after manager callbacks).
	 *
	 * @param server - The server configuration.
	 * @param callbacks - Additional callbacks from the consumer (e.g. session UI updates).
	 * @returns A config object ready to be passed to `TerminalContainer`.
	 */
	buildSctlinConfig(server: ServerConfig, callbacks?: SctlinCallbacks): SctlinConfig {
		const conn = this.connections.get(server.id);
		const maxEntries = this.config.maxActivityEntries;
		const events = this.events;

		return {
			wsUrl: server.wsUrl,
			apiKey: server.apiKey,
			autoConnect: true,
			autoStartSession: false,
			defaultRows: 24,
			defaultCols: 80,
			sessionDefaults: {
				pty: true,
				persistent: true,
				shell: server.shell || undefined,
				workingDir: '~',
				...this.sessionDefaults,
			},
			// Pass the pre-created client so TerminalContainer doesn't create its own
			client: conn?.wsClient,
			callbacks: {
				onConnectionChange: (status) => {
					// Manager already handles status via wsClient.onStatusChange —
					// only forward to consumer callbacks here (no double-fire).
					callbacks?.onConnectionChange?.(status);
				},
				onRemoteSessions: (sessions) => {
					callbacks?.onRemoteSessions?.(sessions);
				},
				onSessionsChange: (sessions) => {
					callbacks?.onSessionsChange?.(sessions);
				},
				onActiveSessionChange: (key) => {
					callbacks?.onActiveSessionChange?.(key);
				},
				onSplitGroupsChange: (groups) => {
					callbacks?.onSplitGroupsChange?.(groups);
				},
				onFocusedPaneChange: (pane) => {
					callbacks?.onFocusedPaneChange?.(pane);
				},
				onActivity: (entry) => {
					if (conn) {
						// Deduplicate: REST fetch on connect may overlap with WS broadcast
						if (conn.activity.some((e) => e.id === entry.id)) return;
						const updated = [...conn.activity, entry];
						conn.activity = updated.length > maxEntries ? updated.slice(-maxEntries) : updated;
						events.onActivity?.(server.id, conn.activity);
						events.onActivityNew?.(server.id, entry);
					}
					callbacks?.onActivity?.(entry);
				},
				onError: (err) => {
					events.onError?.(server.id, new Error(err.message));
					callbacks?.onError?.(err);
				},
				onSessionStarted: (session) => {
					callbacks?.onSessionStarted?.(session);
				},
				onSessionClosed: (sessionId, reason) => {
					callbacks?.onSessionClosed?.(sessionId, reason);
				},
				onAiPermissionChange: (sessionId, allowed) => {
					callbacks?.onAiPermissionChange?.(sessionId, allowed);
				},
				onAiStatusChange: (sessionId, working, activity, message) => {
					callbacks?.onAiStatusChange?.(sessionId, working, activity, message);
				},
				onResize: (sessionId, rows, cols) => {
					callbacks?.onResize?.(sessionId, rows, cols);
				},
			}
		};
	}

	/**
	 * Fetch device info for a connected server.
	 * Updates the connection's `deviceInfo` field and emits `onDeviceInfo`.
	 */
	async fetchDeviceInfo(serverId: string): Promise<DeviceInfo | null> {
		const conn = this.connections.get(serverId);
		if (!conn) return null;
		try {
			const info = await conn.restClient.getInfo();
			conn.deviceInfo = info;
			this.events.onDeviceInfo?.(serverId, info);
			// Log device info summary
			this.logEvent(serverId, 'info', `device: ${info.hostname}`,
				`${info.serial}, up ${this.formatDuration(info.system_uptime_secs * 1000)}, ${info.kernel}`);
			// Log LTE signal if available
			if (info.lte) {
				const parts: string[] = [];
				if (info.lte.operator) parts.push(info.lte.operator);
				if (info.lte.technology) parts.push(info.lte.technology);
				if (info.lte.signal_bars != null) parts.push(`${info.lte.signal_bars}/5 bars`);
				if (info.lte.rsrp != null) parts.push(`RSRP ${info.lte.rsrp}`);
				if (info.lte.sinr != null) parts.push(`SINR ${info.lte.sinr}`);
				if (info.lte.band) parts.push(info.lte.band);
				if (info.lte.modem?.model) parts.push(info.lte.modem.model);
				this.logEvent(serverId, 'info', 'lte signal', parts.join(' | '));
			}
			// Log GPS if available
			if (info.gps) {
				const parts: string[] = [info.gps.status];
				if (info.gps.satellites != null) parts.push(`${info.gps.satellites} sats`);
				if (info.gps.latitude != null && info.gps.longitude != null) {
					parts.push(`${info.gps.latitude.toFixed(4)}, ${info.gps.longitude.toFixed(4)}`);
				}
				this.logEvent(serverId, 'info', 'gps', parts.join(' | '));
			}
			// Log tunnel status if available
			if (info.tunnel) {
				this.logEvent(serverId, 'info', 'tunnel',
					`${info.tunnel.connected ? 'connected' : 'disconnected'} → ${info.tunnel.url}`);
			}
			return info;
		} catch (err) {
			conn.deviceInfo = null;
			this.events.onDeviceInfo?.(serverId, null);
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			this.logEvent(serverId, 'error', 'failed to fetch device info',
				err instanceof Error ? err.message : String(err));
			return null;
		}
	}

	/**
	 * Fetch relay health for a relay connection.
	 * Updates the connection's `relayHealth` field and emits `onRelayHealth`.
	 */
	async fetchRelayHealth(serverId: string): Promise<RelayHealthInfo | null> {
		const conn = this.connections.get(serverId);
		if (!conn || !conn.relayBaseUrl) return null;
		try {
			const resp = await fetch(`${conn.relayBaseUrl}/api/health`, {
				signal: AbortSignal.timeout(5000),
			});
			if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
			const health: RelayHealthInfo = await resp.json();
			conn.relayHealth = health;
			this.events.onRelayHealth?.(serverId, health);

			// Dedup: if health fingerprint unchanged, update existing entries in-place
			const fp = this.relayHealthFingerprint(health);
			const prevFp = this.relayHealthFingerprints.get(serverId);

			if (fp === prevFp) {
				// Move existing relay health entries to end with updated timestamps
				const ids = new Set(this.relayHealthLogIds.get(serverId) ?? []);
				if (ids.size > 0) {
					const now = Date.now();
					const rest: ConnectionEvent[] = [];
					const updated: ConnectionEvent[] = [];
					for (const entry of conn.connectionLog) {
						if (ids.has(entry.id)) {
							// Update detail to reflect current uptime
							let detail = entry.detail;
							if (entry.message.startsWith('relay health:')) {
								const tunnelDetail = health.tunnel.connected
									? `tunnel: connected, ${health.sessions} session(s)`
									: `tunnel: no device, ${health.tunnel.reconnects} reconnect(s)`;
								detail = `v${health.version}, up ${this.formatDuration(health.uptime_secs * 1000)}, ${tunnelDetail}`;
							}
							updated.push({ ...entry, timestamp: now, detail });
						} else {
							rest.push(entry);
						}
					}
					conn.connectionLog = [...rest, ...updated].slice(-200);
					this.events.onConnectionLog?.(serverId, conn.connectionLog);
				}
			} else {
				// Health changed — log new entries
				const startId = this.eventIdCounter + 1;
				const tunnelDetail = health.tunnel.connected
					? `tunnel: connected, ${health.sessions} session(s)`
					: `tunnel: no device, ${health.tunnel.reconnects} reconnect(s)`;
				this.logEvent(serverId, 'info', `relay health: ${health.status}`,
					`v${health.version}, up ${this.formatDuration(health.uptime_secs * 1000)}, ${tunnelDetail}`);
				if (health.lte) {
					const parts: string[] = [];
					if (health.lte.operator) parts.push(health.lte.operator);
					if (health.lte.signal_bars != null) parts.push(`${health.lte.signal_bars}/5 bars`);
					if (health.lte.rsrp != null) parts.push(`RSRP ${health.lte.rsrp}`);
					if (health.lte.sinr != null) parts.push(`SINR ${health.lte.sinr}`);
					if (health.lte.band) parts.push(health.lte.band);
					if (parts.length > 0) {
						this.logEvent(serverId, 'info', 'lte signal', parts.join(' | '));
					}
				}
				if (health.gps) {
					const parts: string[] = [health.gps.status];
					if (health.gps.satellites != null) parts.push(`${health.gps.satellites} sats`);
					if (health.gps.has_fix) parts.push('fix');
					this.logEvent(serverId, 'info', 'gps', parts.join(' | '));
				}
				// Track which IDs belong to this health check
				const ids: number[] = [];
				for (let i = startId; i <= this.eventIdCounter; i++) ids.push(i);
				this.relayHealthLogIds.set(serverId, ids);
				this.relayHealthFingerprints.set(serverId, fp);
			}
			return health;
		} catch (err) {
			conn.relayHealth = null;
			this.events.onRelayHealth?.(serverId, null);
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			this.logEvent(serverId, 'error', 'relay health check failed',
				err instanceof Error ? err.message : String(err));
			// Clear fingerprint on error so next successful check logs fresh
			this.relayHealthFingerprints.delete(serverId);
			this.relayHealthLogIds.delete(serverId);
			return null;
		}
	}

	/**
	 * Fetch system info from the relay server itself.
	 * Requires `relayApiKey` to be configured on the server connection.
	 * Updates the connection's `relayInfo` field and emits `onRelayInfo`.
	 */
	async fetchRelayInfo(serverId: string): Promise<DeviceInfo | null> {
		const conn = this.connections.get(serverId);
		if (!conn?.relayBaseUrl || !conn.relayApiKey) {
			console.warn('[fetchRelayInfo] skipped:', { hasConn: !!conn, relayBaseUrl: conn?.relayBaseUrl, hasRelayApiKey: !!conn?.relayApiKey });
			return null;
		}
		try {
			const resp = await fetch(`${conn.relayBaseUrl}/api/info`, {
				headers: { 'Authorization': `Bearer ${conn.relayApiKey}` },
				signal: AbortSignal.timeout(10_000),
			});
			if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
			const info: DeviceInfo = await resp.json();
			conn.relayInfo = info;
			this.events.onRelayInfo?.(serverId, info);
			return info;
		} catch (err) {
			conn.relayInfo = null;
			this.events.onRelayInfo?.(serverId, null);
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			return null;
		}
	}

	/**
	 * Probe a device through the relay's proxy endpoint.
	 * Fetches `${relayBaseUrl}/d/${serial}/api/health` (unauthenticated).
	 * Returns whether the device is reachable and any error info from the relay.
	 */
	async probeRelayDevice(serverId: string): Promise<DeviceProbeResult | null> {
		const conn = this.connections.get(serverId);
		if (!conn || !conn.relayBaseUrl || !conn.relaySerial) return null;
		this.logEvent(serverId, 'info', `probing device ${conn.relaySerial} through relay...`);
		try {
			const resp = await fetch(`${conn.relayBaseUrl}/d/${conn.relaySerial}/api/health`, {
				signal: AbortSignal.timeout(10_000),
			});
			let errorCode: string | null = null;
			let errorMessage: string | null = null;
			if (!resp.ok) {
				try {
					const body = await resp.json();
					errorCode = body.code ?? null;
					errorMessage = body.message ?? null;
				} catch { /* non-JSON response */ }
			}
			const result: DeviceProbeResult = {
				reachable: resp.ok,
				status: resp.status,
				errorCode,
				errorMessage,
				probedAt: Date.now(),
			};
			conn.deviceProbe = result;
			this.events.onDeviceProbe?.(serverId, result);
			if (result.reachable) {
				this.logEvent(serverId, 'success', 'device reachable through relay');
			} else {
				this.logEvent(serverId, 'warn', `device probe: ${errorCode ?? `HTTP ${resp.status}`}`,
					errorMessage ?? undefined);
			}
			return result;
		} catch (err) {
			const result: DeviceProbeResult = {
				reachable: false,
				status: null,
				errorCode: 'NETWORK_ERROR',
				errorMessage: err instanceof Error ? err.message : String(err),
				probedAt: Date.now(),
			};
			conn.deviceProbe = result;
			this.events.onDeviceProbe?.(serverId, result);
			this.logEvent(serverId, 'error', 'device probe failed',
				err instanceof Error ? err.message : String(err));
			return result;
		}
	}

	/**
	 * Fetch activity log for a connected server.
	 * Updates the connection's `activity` field and emits `onActivity`.
	 */
	async fetchActivity(serverId: string, sinceId = 0, limit = 100): Promise<ActivityEntry[]> {
		const conn = this.connections.get(serverId);
		if (!conn) return [];
		try {
			const entries = await conn.restClient.getActivity(sinceId, limit);
			conn.activity = entries;
			this.events.onActivity?.(serverId, entries);
			return entries;
		} catch (err) {
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			return [];
		}
	}

	/**
	 * Fetch server-side diagnostics (process, system, network, logs).
	 * Returns the diagnostics response, or null on failure.
	 */
	async fetchDiagnostics(serverId: string, logLines = 200, logSince = '24h'): Promise<ServerDiagnostics | null> {
		const conn = this.connections.get(serverId);
		if (!conn) return null;
		try {
			return await conn.restClient.getDiagnostics(logLines, logSince);
		} catch (err) {
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			return null;
		}
	}

	/**
	 * Fetch diagnostics from the relay server itself (not the device).
	 * Requires `relayApiKey` to be configured on the server connection.
	 */
	async fetchRelayDiagnostics(serverId: string, logLines = 200, logSince = '24h'): Promise<ServerDiagnostics | null> {
		const conn = this.connections.get(serverId);
		if (!conn?.relayBaseUrl || !conn.relayApiKey) {
			console.warn('[fetchRelayDiagnostics] skipped:', { hasConn: !!conn, relayBaseUrl: conn?.relayBaseUrl, hasRelayApiKey: !!conn?.relayApiKey });
			return null;
		}
		try {
			const url = `${conn.relayBaseUrl}/api/diagnostics?log_lines=${logLines}&log_since=${logSince}`;
			const resp = await fetch(url, {
				headers: { 'Authorization': `Bearer ${conn.relayApiKey}` },
				signal: AbortSignal.timeout(10_000),
			});
			if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
			return await resp.json();
		} catch (err) {
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			return null;
		}
	}

	/** Disconnect all servers. */
	disconnectAll(): void {
		for (const id of [...this.connections.keys()]) {
			this.disconnect(id);
		}
	}

	/** Disconnect all servers and release all resources. */
	destroy(): void {
		this.disconnectAll();
	}
}
