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
import type {
	ConnectionStatus,
	ReconnectConfig,
	SessionStartOptions,
	ServerConfig,
	DeviceInfo,
	ActivityEntry,
	SctlinConfig,
	SctlinCallbacks
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

		const conn: ServerConnection = {
			id: server.id,
			config: server,
			wsClient,
			restClient,
			transferTracker,
			status: 'connecting',
			deviceInfo: null,
			activity: [],
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
			conn.status = status;
			this.events.onConnectionChange?.(server.id, status);

			// Auto-fetch on first connect
			if (status === 'connected') {
				if (this.config.autoFetchInfo) {
					this.fetchDeviceInfo(server.id).catch(() => {});
				}
				if (this.config.autoFetchActivity) {
					this.fetchActivity(server.id).catch(() => {});
				}
			}
		}));

		this.connections.set(server.id, conn);
		this.unsubscribers.set(server.id, unsubs);

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
			return info;
		} catch (err) {
			conn.deviceInfo = null;
			this.events.onDeviceInfo?.(serverId, null);
			this.events.onError?.(serverId, err instanceof Error ? err : new Error(String(err)));
			return null;
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
