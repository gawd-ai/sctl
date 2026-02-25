import type {
	ConnectionStatus,
	ReconnectConfig,
	SessionStartOptions,
	WsClientMsg,
	WsServerMsg,
	WsSessionStartedMsg,
	WsSessionAttachedMsg,
	WsSessionOutputMsg,
	WsSessionClosedMsg,
	WsSessionExitedMsg,
	WsErrorMsg,
	WsSessionResizeAckMsg,
	WsSessionRenameAckMsg,
	WsSessionAllowAiAckMsg,
	WsSessionListedMsg,
	WsShellListedMsg
} from '../types/terminal.types';
import { ConnectionError, ServerError, TimeoutError } from './errors';

type ServerMsgType = WsServerMsg['type'];
type MsgOfType<T extends ServerMsgType> = Extract<WsServerMsg, { type: T }>;
type Listener<T extends ServerMsgType> = (msg: MsgOfType<T>) => void;

const DEFAULT_RECONNECT: ReconnectConfig = {
	enabled: true,
	initialDelay: 100,
	maxDelay: 2_000,
	maxAttempts: Infinity
};

const DEFAULT_ACK_TIMEOUT_MS = 10_000;
const DEFAULT_PING_INTERVAL_MS = 30_000;

/** Configuration for WebSocket client behavior. */
export interface WsClientConfig {
	/** Interval in ms between keepalive pings. Default: 30000. */
	pingInterval?: number;
	/** Timeout in ms to wait for an ack response. Default: 10000. */
	ackTimeout?: number;
}

/**
 * Framework-agnostic WebSocket client for sctl.
 *
 * Handles connection lifecycle, request/ack correlation, reconnect with
 * exponential backoff, and typed event dispatch.
 */
export class SctlWsClient {
	private ws: WebSocket | null = null;
	private _status: ConnectionStatus = 'disconnected';
	private statusListeners = new Set<(s: ConnectionStatus) => void>();
	private listeners = new Map<string, Set<(msg: never) => void>>();
	private pendingAcks = new Map<string, { resolve: (msg: WsServerMsg) => void; reject: (err: Error) => void; timer: ReturnType<typeof setTimeout> }>();
	private reconnectAttempt = 0;
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	private requestCounter = 0;
	private intentionalClose = false;
	private pingInterval: ReturnType<typeof setInterval> | null = null;
	private _reconnectCount = 0;
	private visibilityHandler: (() => void) | null = null;

	readonly wsUrl: string;
	readonly apiKey: string;
	readonly reconnectConfig: ReconnectConfig;
	private readonly ackTimeoutMs: number;
	private readonly pingIntervalMs: number;

	constructor(wsUrl: string, apiKey: string, reconnect?: Partial<ReconnectConfig>, config?: WsClientConfig) {
		this.wsUrl = wsUrl;
		this.apiKey = apiKey;
		this.reconnectConfig = { ...DEFAULT_RECONNECT, ...reconnect };
		this.ackTimeoutMs = config?.ackTimeout ?? DEFAULT_ACK_TIMEOUT_MS;
		this.pingIntervalMs = config?.pingInterval ?? DEFAULT_PING_INTERVAL_MS;
		// Immediately retry when tab becomes visible during reconnect
		this.visibilityHandler = () => {
			if (document.visibilityState === 'visible' && this._status === 'reconnecting') {
				if (this.reconnectTimer) {
					clearTimeout(this.reconnectTimer);
					this.reconnectTimer = null;
				}
				this.connect();
			}
		};
		if (typeof document !== 'undefined') {
			document.addEventListener('visibilitychange', this.visibilityHandler);
		}
	}

	// ── Connection ──────────────────────────────────────────────────

	/** Current connection status (`'disconnected'`, `'connecting'`, `'connected'`, `'reconnecting'`). */
	get status(): ConnectionStatus {
		return this._status;
	}

	/** Number of reconnect cycles since the last successful connect. */
	get reconnectCount(): number {
		return this._reconnectCount;
	}

	/** Open the WebSocket connection. No-op if already connecting or connected. */
	connect(): void {
		if (this.ws && (this.ws.readyState === WebSocket.CONNECTING || this.ws.readyState === WebSocket.OPEN)) {
			return;
		}
		this.intentionalClose = false;
		this.setStatus('connecting');

		const sep = this.wsUrl.includes('?') ? '&' : '?';
		const url = `${this.wsUrl}${sep}token=${encodeURIComponent(this.apiKey)}`;
		const ws = new WebSocket(url);

		ws.onopen = () => {
			if (this.reconnectAttempt > 0) {
				this._reconnectCount++;
			}
			this.reconnectAttempt = 0;
			this.setStatus('connected');
			this.startPing();
		};

		ws.onmessage = (event) => {
			try {
				const msg: WsServerMsg = JSON.parse(event.data as string);
				this.dispatch(msg);
			} catch {
				// ignore non-JSON frames
			}
		};

		ws.onclose = () => {
			this.ws = null;
			this.stopPing();
			if (this.intentionalClose) {
				this.setStatus('disconnected');
			} else {
				this.scheduleReconnect();
			}
		};

		ws.onerror = (event) => {
			console.error('[SctlWsClient] WebSocket error:', event);
			// onclose will fire after onerror — reconnect handled there
		};

		this.ws = ws;
	}

	/** Close the WebSocket connection, cancel reconnect, and reject all pending acks. */
	disconnect(): void {
		this.intentionalClose = true;
		this.stopPing();
		if (this.reconnectTimer) {
			clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		if (this.visibilityHandler && typeof document !== 'undefined') {
			document.removeEventListener('visibilitychange', this.visibilityHandler);
			this.visibilityHandler = null;
		}
		this.ws?.close();
		this.ws = null;
		this.setStatus('disconnected');
		// Reject all pending acks
		for (const [, pending] of this.pendingAcks) {
			clearTimeout(pending.timer);
			pending.reject(new ConnectionError('Connection closed'));
		}
		this.pendingAcks.clear();
	}

	private scheduleReconnect(): void {
		if (!this.reconnectConfig.enabled || this.reconnectAttempt >= this.reconnectConfig.maxAttempts) {
			this.setStatus('disconnected');
			return;
		}
		this.setStatus('reconnecting');
		// First attempt is immediate, then exponential backoff
		const delay = this.reconnectAttempt === 0
			? 0
			: Math.min(
				this.reconnectConfig.initialDelay * Math.pow(2, this.reconnectAttempt - 1),
				this.reconnectConfig.maxDelay
			);
		this.reconnectAttempt++;
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = null;
			this.connect();
		}, delay);
	}

	private setStatus(s: ConnectionStatus): void {
		if (this._status === s) return;
		this._status = s;
		for (const cb of this.statusListeners) cb(s);
	}

	/** Register a callback for connection status changes. Returns an unsubscribe function. */
	onStatusChange(cb: (s: ConnectionStatus) => void): () => void {
		this.statusListeners.add(cb);
		return () => this.statusListeners.delete(cb);
	}

	// ── Event dispatch ──────────────────────────────────────────────

	private dispatch(msg: WsServerMsg): void {
		// Resolve pending ack if request_id matches
		if ('request_id' in msg && msg.request_id) {
			const pending = this.pendingAcks.get(msg.request_id);
			if (pending) {
				clearTimeout(pending.timer);
				this.pendingAcks.delete(msg.request_id);
				if (msg.type === 'error') {
					const errMsg = msg as WsErrorMsg;
					pending.reject(new ServerError(errMsg.code, errMsg.message));
				} else {
					pending.resolve(msg);
				}
			}
		}

		// Emit to typed listeners
		const set = this.listeners.get(msg.type);
		if (set) {
			for (const cb of set) (cb as (msg: WsServerMsg) => void)(msg);
		}
	}

	/** Subscribe to a specific message type. Returns an unsubscribe function. */
	on<T extends ServerMsgType>(type: T, cb: Listener<T>): () => void {
		if (!this.listeners.has(type)) this.listeners.set(type, new Set());
		const set = this.listeners.get(type)!;
		set.add(cb as (msg: never) => void);
		return () => set.delete(cb as (msg: never) => void);
	}

	/** Convenience: subscribe to session output (stdout/stderr/system) for a specific session. */
	onOutput(sessionId: string, cb: (msg: WsSessionOutputMsg) => void): () => void {
		const handler = (msg: WsServerMsg) => {
			const m = msg as WsSessionOutputMsg;
			if (m.session_id === sessionId) cb(m);
		};
		const types: ServerMsgType[] = ['session.stdout', 'session.stderr', 'session.system'];
		const unsubs = types.map((type) => {
			if (!this.listeners.has(type)) this.listeners.set(type, new Set());
			const set = this.listeners.get(type)!;
			set.add(handler as (msg: never) => void);
			return () => set.delete(handler as (msg: never) => void);
		});
		return () => unsubs.forEach((u) => u());
	}

	// ── Send helpers ────────────────────────────────────────────────

	private nextRequestId(): string {
		return `req_${++this.requestCounter}_${Date.now()}`;
	}

	private send(msg: WsClientMsg): void {
		if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
			throw new ConnectionError('WebSocket not connected');
		}
		this.ws.send(JSON.stringify(msg));
	}

	/**
	 * Send a message and wait for the correlated ack (matched by request_id).
	 * Rejects on timeout or if the server responds with an error.
	 */
	private sendWithAck<T extends WsServerMsg>(msg: WsClientMsg & { request_id?: string }): Promise<T> {
		const requestId = this.nextRequestId();
		const tagged = { ...msg, request_id: requestId };

		return new Promise<T>((resolve, reject) => {
			const timer = setTimeout(() => {
				this.pendingAcks.delete(requestId);
				reject(new TimeoutError(`Ack timeout for ${msg.type} (${requestId})`));
			}, this.ackTimeoutMs);

			this.pendingAcks.set(requestId, {
				resolve: resolve as (msg: WsServerMsg) => void,
				reject,
				timer
			});

			try {
				this.send(tagged);
			} catch (err) {
				clearTimeout(timer);
				this.pendingAcks.delete(requestId);
				reject(err);
			}
		});
	}

	// ── Session operations ──────────────────────────────────────────

	/** Start a new shell session on the device. Returns the session ID and metadata. */
	async startSession(opts?: SessionStartOptions): Promise<WsSessionStartedMsg> {
		return this.sendWithAck<WsSessionStartedMsg>({
			type: 'session.start',
			working_dir: opts?.workingDir,
			persistent: opts?.persistent,
			env: opts?.env,
			shell: opts?.shell,
			pty: opts?.pty ?? true,
			rows: opts?.rows,
			cols: opts?.cols,
			name: opts?.name
		});
	}

	/** Attach to an existing session, replaying output since the given sequence number. */
	async attachSession(sessionId: string, since?: number): Promise<WsSessionAttachedMsg> {
		return this.sendWithAck<WsSessionAttachedMsg>({
			type: 'session.attach',
			session_id: sessionId,
			since
		});
	}

	/** Kill a session and its process group. */
	async killSession(sessionId: string): Promise<WsSessionClosedMsg> {
		return this.sendWithAck<WsSessionClosedMsg>({
			type: 'session.kill',
			session_id: sessionId
		});
	}

	/** Fire-and-forget stdin data (hot path for keystrokes — no ack). */
	sendStdin(sessionId: string, data: string): void {
		try {
			this.send({
				type: 'session.stdin',
				session_id: sessionId,
				data
			});
		} catch {
			// Silently drop keystrokes when disconnected
		}
	}

	/** Execute a command in a session (sends command + Enter). */
	async execCommand(sessionId: string, command: string): Promise<void> {
		await this.sendWithAck({
			type: 'session.exec',
			session_id: sessionId,
			command
		});
	}

	/** Send a POSIX signal to a session's process group (e.g. 2 for SIGINT). */
	async sendSignal(sessionId: string, signal: number): Promise<void> {
		await this.sendWithAck({
			type: 'session.signal',
			session_id: sessionId,
			signal
		});
	}

	/** Resize a session's terminal (PTY dimensions). */
	async resizeSession(sessionId: string, rows: number, cols: number): Promise<WsSessionResizeAckMsg> {
		return this.sendWithAck<WsSessionResizeAckMsg>({
			type: 'session.resize',
			session_id: sessionId,
			rows,
			cols
		});
	}

	/** List all sessions on the device. */
	async listSessions(): Promise<WsSessionListedMsg> {
		return this.sendWithAck<WsSessionListedMsg>({
			type: 'session.list'
		});
	}

	/** List available shells on the device. */
	async listShells(): Promise<WsShellListedMsg> {
		return this.sendWithAck<WsShellListedMsg>({
			type: 'shell.list'
		});
	}

	/** Rename a session (human-readable label). */
	async renameSession(sessionId: string, name: string): Promise<WsSessionRenameAckMsg> {
		return this.sendWithAck<WsSessionRenameAckMsg>({
			type: 'session.rename',
			session_id: sessionId,
			name
		});
	}

	/** Set whether AI agents are permitted to control a session. */
	async setUserAllowsAi(sessionId: string, allowed: boolean): Promise<WsSessionAllowAiAckMsg> {
		return this.sendWithAck<WsSessionAllowAiAckMsg>({
			type: 'session.allow_ai',
			session_id: sessionId,
			allowed
		});
	}

	// ── Convenience for orchestration ───────────────────────────────

	/** Subscribe to session.closed and session.exited events for a session. */
	onSessionEnd(sessionId: string, cb: (msg: WsSessionClosedMsg | WsSessionExitedMsg) => void): () => void {
		const unsubs = [
			this.on('session.closed', (m) => { if (m.session_id === sessionId) cb(m); }),
			this.on('session.exited', (m) => { if (m.session_id === sessionId) cb(m); })
		];
		return () => unsubs.forEach((u) => u());
	}

	// ── Ping keepalive ──────────────────────────────────────────────

	private startPing(): void {
		this.stopPing();
		this.pingInterval = setInterval(() => {
			try {
				this.send({ type: 'ping' });
			} catch {
				// Connection lost — onclose will handle reconnect
			}
		}, this.pingIntervalMs);
	}

	private stopPing(): void {
		if (this.pingInterval) {
			clearInterval(this.pingInterval);
			this.pingInterval = null;
		}
	}
}
