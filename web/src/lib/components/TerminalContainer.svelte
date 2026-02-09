<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import type {
		SctlinConfig,
		SessionInfo,
		ConnectionStatus,
		WsSessionOutputMsg,
		RemoteSessionInfo,
		WsSessionCreatedBroadcast,
		WsSessionDestroyedBroadcast,
		WsSessionRenamedBroadcast,
		WsSessionAiPermissionChangedBroadcast,
		WsSessionAiStatusChangedBroadcast
	} from '../types/terminal.types';
	import { SctlWsClient } from '../utils/ws-client';
	import Terminal from './Terminal.svelte';
	import TerminalTabs from './TerminalTabs.svelte';
	import ControlBar from './ControlBar.svelte';

	interface Props {
		config: SctlinConfig;
		showTabs?: boolean;
	}

	let { config, showTabs = true }: Props = $props();

	let sessions: SessionInfo[] = $state([]);
	let activeKey: string | null = $state(null);
	let connectionStatus: ConnectionStatus = $state('disconnected');
	let containerError: string | null = $state(null);
	let terminalRows = $state(0);
	let terminalCols = $state(0);
	let remoteSessions: RemoteSessionInfo[] = $state([]);

	// Plain object for Terminal component references — NOT $state.
	// Indexed by session.key (locally unique UUID).
	let terminalRefs: Record<string, Terminal | undefined> = {};

	// Fast lookup: server sessionId → local key (for server message handlers)
	const keyForSession = new Map<string, string>();

	// Non-reactive seq tracking — avoids re-rendering on every output message
	const seqMap = new Map<string, number>();

	// Buffer for output that arrives before xterm is ready inside the Terminal.
	const outputBuffer = new Map<string, string[]>();

	// Suppress xterm input responses while replaying old output on reattach.
	const suppressedInput = new Set<string>();
	const suppressOnFlush = new Set<string>();

	// Store unsubscribe functions per session for attach/detach lifecycle
	const subscriptionCleanups = new Map<string, (() => void)[]>();

	let sessionCounter = 0;
	let client: SctlWsClient;

	const SESSION_START_TIMEOUT_MS = 15_000;

	function activeSession(): SessionInfo | undefined {
		return sessions.find((s) => s.key === activeKey);
	}

	function setActiveKey(key: string): void {
		activeKey = key;
		setTimeout(() => terminalRefs[key]?.focus(), 50);
	}

	/** Resolve a local key to its server sessionId. */
	function sessionIdFor(key: string): string | undefined {
		return sessions.find((s) => s.key === key)?.sessionId;
	}

	/** Get the Terminal ref for a session by its server sessionId. */
	function getTermRef(sessionId: string): Terminal | undefined {
		return terminalRefs[keyForSession.get(sessionId) ?? ''];
	}

	// ── Session management ──────────────────────────────────────────

	export async function listShells(): Promise<{ shells: string[]; defaultShell: string }> {
		const result = await client.listShells();
		return { shells: result.shells, defaultShell: result.default_shell };
	}

	export async function startSession(shell?: string): Promise<void> {
		containerError = null;
		try {
			const defaults = config.sessionDefaults ?? {};

			sessionCounter++;
			const shellName = (shell ?? defaults.shell ?? '').split('/').pop() || 'sh';
			let host = '';
			try { host = new URL(config.wsUrl).hostname; } catch { host = config.wsUrl; }
			const label = `${host}-${sessionCounter}-${shellName}`;

			const startPromise = client.startSession({
				pty: true,
				rows: config.defaultRows ?? (terminalRows || 24),
				cols: config.defaultCols ?? (terminalCols || 80),
				...defaults,
				shell: shell ?? defaults.shell,
				env: { TERM: 'xterm-256color', ...defaults.env },
				name: label
			});

			const timeoutPromise = new Promise<never>((_, reject) =>
				setTimeout(() => reject(new Error('Session start timed out (15s)')), SESSION_START_TIMEOUT_MS)
			);

			const result = await Promise.race([startPromise, timeoutPromise]);

			const serverAllowsAi = (result as Record<string, unknown>).user_allows_ai as boolean | undefined;
			const serverName = (result as Record<string, unknown>).name as string | undefined;
			const key = crypto.randomUUID();
			const session: SessionInfo = {
				key,
				sessionId: result.session_id,
				pid: result.pid,
				persistent: defaults.persistent ?? false,
				pty: result.pty,
				userAllowsAi: serverAllowsAi ?? true,
				aiIsWorking: false,
				lastSeq: 0,
				label: serverName || label,
				attached: true
			};

			keyForSession.set(session.sessionId, key);
			sessions = [...sessions, session];
			setActiveKey(key);

			// Subscribe to output (buffers if Terminal hasn't mounted yet)
			subscribeSession(session.sessionId);

			// Flush attempts: tick waits for Svelte render (bind:this),
			// onready (in template) fires when xterm is fully loaded.
			tick().then(() => flushBuffer(session.sessionId));

			config.callbacks?.onSessionStarted?.(session);
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to start session';
			containerError = msg;
			config.callbacks?.onError?.({ type: 'error', code: 'session_start_failed', message: msg });
			console.error('Failed to start session:', err);
		}
	}

	/** Attach to a server session by its server-assigned sessionId.
	 *  Accepts sessionId because the session may not be local yet. */
	export async function attachSession(sessionId: string): Promise<void> {
		try {
			const existing = sessions.find((s) => s.sessionId === sessionId);
			if (existing?.dead) return; // Dead sessions cannot be re-attached
			const since = seqMap.get(sessionId) ?? 0;

			const result = await client.attachSession(sessionId, since);

			if (existing) {
				// Tab already exists — just subscribe and mark attached
				if (!existing.attached) {
					subscribeSession(sessionId);
					sessions = sessions.map((s) =>
						s.key === existing.key ? { ...s, attached: true } : s
					);
					setActiveKey(existing.key);
					await tick();
					syncTerminalSize(sessionId, true);
				}

				// Replay buffered entries (Terminal is mounted)
				if (result.entries && result.entries.length > 0) {
					suppressInputForReplay(sessionId);
					const ref = getTermRef(sessionId);
					let maxSeq = since;
					for (const entry of result.entries) {
						ref?.write(entry.data);
						if (entry.seq > maxSeq) maxSeq = entry.seq;
					}
					updateSessionSeq(sessionId, maxSeq);
				}
			} else {
				// No tab yet — create one and subscribe.
				const remote = remoteSessions.find((r) => r.session_id === sessionId);
				const key = crypto.randomUUID();
				const session: SessionInfo = {
					key,
					sessionId,
					persistent: true,
					pty: true,
					userAllowsAi: remote?.user_allows_ai ?? true,
					aiIsWorking: remote?.ai_is_working ?? false,
					aiActivity: remote?.ai_activity,
					aiStatusMessage: remote?.ai_status_message,
					lastSeq: 0,
					label: remote?.name,
					attached: true
				};
				keyForSession.set(sessionId, key);
				sessions = [...sessions, session];
				setActiveKey(key);
				subscribeSession(sessionId);

				if (result.entries && result.entries.length > 0) {
					let buf = outputBuffer.get(sessionId);
					if (!buf) {
						buf = [];
						outputBuffer.set(sessionId, buf);
					}
					let maxSeq = since;
					for (const entry of result.entries) {
						buf.push(entry.data);
						if (entry.seq > maxSeq) maxSeq = entry.seq;
					}
					updateSessionSeq(sessionId, maxSeq);
				}

				suppressOnFlush.add(sessionId);
				tick().then(() => flushBuffer(sessionId));
			}
		} catch (err) {
			console.error('Failed to attach session:', err);
			// If attach fails, the session is likely gone — mark dead
			if (existing) {
				unsubscribeSession(sessionId);
				sessions = sessions.map((s) =>
					s.sessionId === sessionId
						? { ...s, dead: true, attached: false, aiIsWorking: false, aiActivity: undefined, aiStatusMessage: undefined }
						: s
				);
			}
		}
	}

	/** Open a tab for a server session (read-only until attached).
	 *  Accepts sessionId because the session may not be local yet. */
	export async function openTab(sessionId: string): Promise<void> {
		const existing = sessions.find((s) => s.sessionId === sessionId);
		if (existing) {
			// Tab already exists — just select it
			setActiveKey(existing.key);
			return;
		}
		// Look up the remote session name for the tab label
		const remote = remoteSessions.find((r) => r.session_id === sessionId);
		const key = crypto.randomUUID();
		const session: SessionInfo = {
			key,
			sessionId,
			persistent: true,
			pty: true,
			userAllowsAi: remote?.user_allows_ai ?? false,
			aiIsWorking: remote?.ai_is_working ?? false,
			aiActivity: remote?.ai_activity,
			aiStatusMessage: remote?.ai_status_message,
			lastSeq: 0,
			label: remote?.name,
			attached: false
		};
		keyForSession.set(sessionId, key);
		sessions = [...sessions, session];
		setActiveKey(key);

		// Fetch output history for read-only viewing
		try {
			const result = await client.attachSession(sessionId, 0);
			if (result.entries && result.entries.length > 0) {
				let buf = outputBuffer.get(sessionId);
				if (!buf) {
					buf = [];
					outputBuffer.set(sessionId, buf);
				}
				let maxSeq = 0;
				for (const entry of result.entries) {
					buf.push(entry.data);
					if (entry.seq > maxSeq) maxSeq = entry.seq;
				}
				updateSessionSeq(sessionId, maxSeq);
				tick().then(() => flushBuffer(sessionId));
			}
		} catch {
			// History fetch failed — tab opens empty
		}
	}

	// ── Public methods (key-based) ──────────────────────────────────
	// All accept session.key (globally unique UUID).

	export function selectSession(key: string): void {
		if (sessions.some((s) => s.key === key)) setActiveKey(key);
	}

	export async function closeSession(key: string): Promise<void> {
		const sid = sessionIdFor(key);
		if (!sid) return;
		try {
			await client.killSession(sid);
		} catch {
			// Session may already be dead — remove locally anyway
		}
		removeSession(sid);
		fetchRemoteSessions();
	}

	export function detachSession(key: string): void {
		const sid = sessionIdFor(key);
		if (!sid) return;
		unsubscribeSession(sid);
		sessions = sessions.map((s) =>
			s.key === key ? { ...s, attached: false } : s
		);
		fetchRemoteSessions();
	}

	export function closeTab(key: string): void {
		const sid = sessionIdFor(key);
		if (!sid) return;
		unsubscribeSession(sid);
		removeSession(sid);
		fetchRemoteSessions();
	}

	export function renameSession(key: string, label: string): void {
		const sid = sessionIdFor(key);
		if (!sid) return;
		sessions = sessions.map((s) =>
			s.key === key ? { ...s, label } : s
		);
		client.renameSession(sid, label).catch(() => {});
	}

	// ── Internal helpers (sessionId-based, for server communication) ─

	function removeSession(sessionId: string): void {
		const key = keyForSession.get(sessionId);
		if (key) {
			delete terminalRefs[key];
			keyForSession.delete(sessionId);
		}
		seqMap.delete(sessionId);
		outputBuffer.delete(sessionId);
		subscriptionCleanups.delete(sessionId);
		sessions = sessions.filter((s) => s.sessionId !== sessionId);
		if (activeKey === key) {
			activeKey = sessions.length > 0 ? sessions[sessions.length - 1].key : null;
		}
		config.callbacks?.onSessionClosed?.(sessionId, 'closed');
	}

	function subscribeSession(sessionId: string): void {
		// Clean up any existing subscriptions first
		unsubscribeSession(sessionId);
		const cleanups: (() => void)[] = [];

		cleanups.push(client.onOutput(sessionId, (msg: WsSessionOutputMsg) => {
			const ref = getTermRef(sessionId);
			if (ref) {
				// Flush any earlier buffered output, then write new data
				const buf = outputBuffer.get(sessionId);
				if (buf) {
					for (const data of buf) ref.write(data);
					outputBuffer.delete(sessionId);
				}
				ref.write(msg.data);
			} else {
				// Terminal component hasn't mounted yet — buffer the output
				let buf = outputBuffer.get(sessionId);
				if (!buf) {
					buf = [];
					outputBuffer.set(sessionId, buf);
				}
				buf.push(msg.data);
			}
			updateSessionSeq(sessionId, msg.seq);
		}));

		cleanups.push(client.onSessionEnd(sessionId, () => {
			outputBuffer.delete(sessionId);
			removeSession(sessionId);
		}));

		subscriptionCleanups.set(sessionId, cleanups);
	}

	function unsubscribeSession(sessionId: string): void {
		const cleanups = subscriptionCleanups.get(sessionId);
		if (cleanups) {
			for (const fn of cleanups) fn();
			subscriptionCleanups.delete(sessionId);
		}
	}

	function flushBuffer(sessionId: string): void {
		const buf = outputBuffer.get(sessionId);
		const ref = getTermRef(sessionId);
		if (suppressOnFlush.has(sessionId)) {
			suppressOnFlush.delete(sessionId);
			suppressInputForReplay(sessionId);
		}
		if (buf && ref) {
			for (const data of buf) ref.write(data);
			outputBuffer.delete(sessionId);
		}
	}

	function suppressInputForReplay(sessionId: string): void {
		suppressedInput.add(sessionId);
		setTimeout(() => {
			suppressedInput.delete(sessionId);
		}, 50);
	}

	function syncTerminalSize(sessionId: string, attached = true): void {
		if (!attached) return;
		const ref = getTermRef(sessionId);
		if (!ref) return;
		ref.fit();
		const size = ref.getSize();
		if (size) handleTerminalResize(sessionId, size.rows, size.cols);
	}

	function updateSessionSeq(sessionId: string, seq: number): void {
		const prev = seqMap.get(sessionId) ?? 0;
		seqMap.set(sessionId, Math.max(prev, seq));
	}

	/** Reconcile local sessions with remote server state after reconnect.
	 *  Sessions not on the server are marked dead; sessions on the server
	 *  have their AI state synced. */
	function reconcileWithRemote(remote: RemoteSessionInfo[]): void {
		const remoteIds = new Set(remote.map((r) => r.session_id));
		sessions = sessions.map((s) => {
			if (!remoteIds.has(s.sessionId)) {
				// Session no longer exists on server — mark dead
				unsubscribeSession(s.sessionId);
				return { ...s, dead: true, attached: false, aiIsWorking: false, aiActivity: undefined, aiStatusMessage: undefined };
			}
			// Session exists — sync AI state from server
			const r = remote.find((x) => x.session_id === s.sessionId)!;
			return {
				...s,
				userAllowsAi: r.user_allows_ai ?? s.userAllowsAi,
				aiIsWorking: r.ai_is_working ?? false,
				aiActivity: r.ai_activity,
				aiStatusMessage: r.ai_status_message,
			};
		});
	}

	// ── AI permission ────────────────────────────────────────────────

	async function toggleUserAllowsAi(): Promise<void> {
		const session = activeSession();
		if (!session) return;
		const newAllowed = !session.userAllowsAi;
		try {
			await client.setUserAllowsAi(session.sessionId, newAllowed);
			sessions = sessions.map((s) =>
				s.key === session.key ? { ...s, userAllowsAi: newAllowed } : s
			);
			config.callbacks?.onAiPermissionChange?.(session.sessionId, newAllowed);
		} catch (err) {
			console.error('Failed to set AI permission:', err);
		}
	}

	// ── Terminal callbacks ───────────────────────────────────────────

	// xterm DA (Device Attributes) responses — terminal-level, not user input.
	const DA_RESPONSE_RE = /^\x1b\[[\?>=][\d;]*c$/;

	function handleTerminalData(sessionId: string, data: string): void {
		const session = sessions.find((s) => s.sessionId === sessionId);
		if (suppressedInput.has(sessionId)) return;
		if (!session?.attached || session.aiIsWorking || session.dead) return;
		if (DA_RESPONSE_RE.test(data)) return;
		client.sendStdin(sessionId, data);
	}

	function handleTerminalResize(sessionId: string, rows: number, cols: number): void {
		terminalRows = rows;
		terminalCols = cols;
		if (connectionStatus === 'connected') {
			client.resizeSession(sessionId, rows, cols).catch(() => {});
		}
		config.callbacks?.onResize?.(sessionId, rows, cols);
	}

	async function handleSignal(signal: number): Promise<void> {
		const session = activeSession();
		if (!session) return;
		try {
			await client.sendSignal(session.sessionId, signal);
		} catch (err) {
			console.error('Failed to send signal:', err);
		}
	}

	// ── Public accessors ────────────────────────────────────────────

	export function getSessionList(): SessionInfo[] {
		return sessions.map((s) => ({ ...s, lastSeq: seqMap.get(s.sessionId) ?? s.lastSeq }));
	}

	export function getActiveKey(): string | null {
		return activeKey;
	}

	/** Return the latest list of server-side sessions (fetched on connect). */
	export function getRemoteSessions(): RemoteSessionInfo[] {
		return remoteSessions;
	}

	/** Fetch the list of sessions from the server. */
	export async function fetchRemoteSessions(): Promise<RemoteSessionInfo[]> {
		try {
			const result = await client.listSessions();
			remoteSessions = result.sessions;
			config.callbacks?.onRemoteSessions?.(remoteSessions);
			return remoteSessions;
		} catch (err) {
			console.error('Failed to list remote sessions:', err);
			return [];
		}
	}

	/** Run a one-shot command over the WS (temp non-PTY session). */
	export async function exec(command: string): Promise<string> {
		const sess = await client.startSession({ pty: false });
		const output: string[] = [];
		const unsub = client.onOutput(sess.session_id, (msg) => {
			output.push(msg.data);
		});
		await client.execCommand(sess.session_id, command);
		// Give the server a moment to flush output
		await new Promise((r) => setTimeout(r, 500));
		unsub();
		await client.killSession(sess.session_id).catch(() => {});
		return output.join('');
	}

	// ── Lifecycle ───────────────────────────────────────────────────

	onMount(() => {
		client = new SctlWsClient(config.wsUrl, config.apiKey, config.reconnect);

		client.onStatusChange((status) => {
			connectionStatus = status;
			config.callbacks?.onConnectionChange?.(status);

			if (status === 'connected') {
				// Fetch remote sessions first, then reconcile before re-attaching
				fetchRemoteSessions().then((remote) => {
					if (sessions.length > 0) {
						reconcileWithRemote(remote);
						// Only re-attach non-dead sessions
						for (const session of sessions) {
							if (!session.dead) {
								attachSession(session.sessionId);
							}
						}
					}
				});
			}
		});

		client.on('error', (msg) => {
			config.callbacks?.onError?.(msg);
		});

		// Subscribe to real-time session lifecycle broadcasts
		client.on('session.created', (_msg: WsSessionCreatedBroadcast) => {
			fetchRemoteSessions();
		});

		client.on('session.destroyed', (msg: WsSessionDestroyedBroadcast) => {
			const local = sessions.find((s) => s.sessionId === msg.session_id);
			if (local) {
				removeSession(msg.session_id);
			}
			fetchRemoteSessions();
		});

		client.on('session.ai_permission_changed', (msg: WsSessionAiPermissionChangedBroadcast) => {
			const local = sessions.find((s) => s.sessionId === msg.session_id);
			if (local && local.userAllowsAi !== msg.allowed) {
				sessions = sessions.map((s) =>
					s.sessionId === msg.session_id ? { ...s, userAllowsAi: msg.allowed } : s
				);
				config.callbacks?.onAiPermissionChange?.(msg.session_id, msg.allowed);
			}
		});

		client.on('session.ai_status_changed', (msg: WsSessionAiStatusChangedBroadcast) => {
			sessions = sessions.map((s) =>
				s.sessionId === msg.session_id
					? {
							...s,
							aiIsWorking: msg.working,
							aiActivity: msg.activity,
							aiStatusMessage: msg.message
						}
					: s
			);
			config.callbacks?.onAiStatusChange?.(msg.session_id, msg.working, msg.activity, msg.message);
		});

		client.on('session.renamed', (msg: WsSessionRenamedBroadcast) => {
			remoteSessions = remoteSessions.map((r) =>
				r.session_id === msg.session_id ? { ...r, name: msg.name } : r
			);
			config.callbacks?.onRemoteSessions?.(remoteSessions);
			const local = sessions.find((s) => s.sessionId === msg.session_id);
			if (local) {
				sessions = sessions.map((s) =>
					s.sessionId === msg.session_id ? { ...s, label: msg.name } : s
				);
			}
		});

		if (config.autoConnect !== false) {
			client.connect();
		}

		// Auto-start a session once connected (if configured)
		if (config.autoStartSession !== false) {
			const unsub = client.onStatusChange((status) => {
				if (status === 'connected' && sessions.length === 0) {
					unsub();
					startSession();
				}
			});
		}

		return () => {
			client.disconnect();
		};
	});

	// Push session state to parent via callbacks (single source of truth).
	// untrack around the callback call prevents config (a deep reactive proxy)
	// from being tracked as a dependency, avoiding infinite update loops.
	$effect(() => {
		const snapshot = sessions.map((s) => ({
			...s, lastSeq: seqMap.get(s.sessionId) ?? s.lastSeq
		}));
		untrack(() => config.callbacks?.onSessionsChange?.(snapshot));
	});

	$effect(() => {
		const key = activeKey;
		untrack(() => config.callbacks?.onActiveSessionChange?.(key));
	});

</script>

<div class="sctlin-container flex flex-col h-full text-neutral-200">
	<!-- Tab bar -->
	{#if showTabs && sessions.length > 0}
		<div class="flex items-center border-b border-neutral-700 h-8">
			<div class="flex-1 min-w-0">
				<TerminalTabs
					{sessions}
					activeSessionId={activeKey}
					onselect={(key) => setActiveKey(key)}
					onclose={(key) => closeTab(key)}
					onrename={(key, label) => renameSession(key, label)}
					ondotclick={(key) => {
						const s = sessions.find((x) => x.key === key);
						if (s?.dead) return;
						if (s?.attached) detachSession(key);
						else if (s) attachSession(s.sessionId);
					}}
				/>
			</div>
		</div>
	{/if}

	<!-- Terminal area -->
	<div class="flex-1 relative min-h-0">
		{#if sessions.length > 0}
			<div class="absolute inset-0 pointer-events-none" style="background: rgba(12, 12, 12, 0.85); z-index: 0;"></div>
		{/if}

		{#each sessions as session (session.key)}
			<div
				class="absolute inset-0"
				style:visibility={session.key !== activeKey ? 'hidden' : null}
				style:z-index="1"
			>
				<!-- svelte-ignore binding_property_non_reactive -->
				<Terminal
					theme={config.theme}
					readonly={session.dead || session.aiIsWorking || !session.attached}
					overlayLabel={session.dead ? 'Session Lost' : session.aiIsWorking ? (session.aiActivity === 'read' ? 'AI Reading' : 'AI Executing') : undefined}
					overlayColor={session.dead ? 'gray' : session.aiActivity === 'read' ? 'blue' : 'green'}
					rows={config.defaultRows}
					cols={config.defaultCols}
					ondata={(data) => handleTerminalData(session.sessionId, data)}
					onresize={(r, c) => handleTerminalResize(session.sessionId, r, c)}
					onready={() => {
						flushBuffer(session.sessionId);
						syncTerminalSize(session.sessionId, session.attached);
					}}
					bind:this={terminalRefs[session.key]}
				/>
			</div>
		{/each}
	</div>

	<!-- Control bar — shown whenever a session tab is open -->
	{#if activeSession()}
		<ControlBar
			userAllowsAi={activeSession()?.userAllowsAi ?? false}
			aiIsWorking={activeSession()?.aiIsWorking ?? false}
			aiActivity={activeSession()?.aiActivity}
			aiStatusMessage={activeSession()?.aiStatusMessage}
			disabled={!activeSession()?.attached || activeSession()?.dead === true}
			{terminalRows}
			{terminalCols}
			ontoggleai={toggleUserAllowsAi}
			onsignal={handleSignal}
		/>
	{/if}
</div>

<style>
	.sctlin-container {
		min-height: 200px;
	}
</style>
