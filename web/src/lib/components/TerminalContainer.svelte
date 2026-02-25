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
		WsSessionAiStatusChangedBroadcast,
		WsActivityNewMsg
	} from '../types/terminal.types';
	import { SctlWsClient } from '../utils/ws-client';
	import Terminal from './Terminal.svelte';
	import TerminalTabs from './TerminalTabs.svelte';
	import ControlBar from './ControlBar.svelte';


	interface Props {
		config: SctlinConfig;
		showTabs?: boolean;
		onToggleFiles?: () => void;
		onTogglePlaybooks?: () => void;
		sidePanelOpen?: boolean;
		sidePanelTab?: string;
		rightInset?: number;
		rightInsetAnimate?: boolean;
	}

	let { config, showTabs = true, onToggleFiles = undefined, onTogglePlaybooks = undefined, sidePanelOpen = false, sidePanelTab = '', rightInset = 0, rightInsetAnimate = false }: Props = $props();

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
	let searchVisible = $state(false);

	// Split pane state — supports multiple independent groups
	interface SplitGroup {
		secondaryKey: string;
		direction: 'horizontal' | 'vertical';
		ratio: number;
	}
	// Map from primaryKey → group info
	let splitGroups: Record<string, SplitGroup> = $state({});
	let focusedPane: 'primary' | 'secondary' = $state('primary');
	let splitSetupInProgress = false;
	let draggingSplit = $state(false);
	let terminalAreaEl: HTMLDivElement | undefined = $state();
	let dragCleanup: (() => void) | undefined;
	let splitPickerDirection: 'horizontal' | 'vertical' | null = $state(null);

	/** Get the split group for the current activeKey (if it's a primary). */
	function currentGroup(): SplitGroup | null {
		return activeKey ? splitGroups[activeKey] ?? null : null;
	}

	/** Find the group a key belongs to (as primary or secondary). */
	function groupForKey(key: string): { primaryKey: string; group: SplitGroup } | null {
		if (splitGroups[key]) return { primaryKey: key, group: splitGroups[key] };
		for (const [pk, g] of Object.entries(splitGroups)) {
			if (g.secondaryKey === key) return { primaryKey: pk, group: g };
		}
		return null;
	}

	/** All keys currently in any split group (primary or secondary). */
	function allGroupedKeys(): Set<string> {
		const keys = new Set<string>();
		for (const [pk, g] of Object.entries(splitGroups)) {
			keys.add(pk);
			keys.add(g.secondaryKey);
		}
		return keys;
	}

	const SESSION_START_TIMEOUT_MS = 15_000;

	function activeSession(): SessionInfo | undefined {
		return sessions.find((s) => s.key === activeKey);
	}

	/** Whether the split is currently visible (active tab is a split primary). */
	function isSplitVisible(): boolean {
		return !!activeKey && !!splitGroups[activeKey];
	}

	/** The session in the focused split pane (or the active session if not split). */
	function focusedSession(): SessionInfo | undefined {
		const g = currentGroup();
		if (g && focusedPane === 'secondary') {
			return sessions.find((s) => s.key === g.secondaryKey);
		}
		return activeSession();
	}

	function setActiveKey(key: string): void {
		if (!splitSetupInProgress) {
			const found = groupForKey(key);
			if (found) {
				// Clicking a member of a split group → navigate to that group
				activeKey = found.primaryKey;
				focusedPane = key === found.primaryKey ? 'primary' : 'secondary';
				handlePaneFocus(focusedPane);
				tick().then(() => {
					fitGroupPanes(found.primaryKey);
					terminalRefs[key]?.focus();
				});
				return;
			}
		}
		activeKey = key;
		tick().then(() => {
			terminalRefs[key]?.fit();
			terminalRefs[key]?.focus();
		});
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

			const serverAllowsAi = (result as unknown as Record<string, unknown>).user_allows_ai as boolean | undefined;
			const serverName = (result as unknown as Record<string, unknown>).name as string | undefined;
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
		let existing = sessions.find((s) => s.sessionId === sessionId);

		// If no local tab exists, create one immediately so the user sees feedback
		if (!existing) {
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
				attached: false // not yet attached — will be set true after WS attach
			};
			keyForSession.set(sessionId, key);
			sessions = [...sessions, session];
			setActiveKey(key);
			existing = session;
		} else if (existing.dead) {
			return; // Dead sessions cannot be re-attached
		} else {
			setActiveKey(existing.key);
		}

		// Now attach via WS
		try {
			const since = seqMap.get(sessionId) ?? 0;
			const result = await client.attachSession(sessionId, since);

			// Guard: session may have been removed or marked dead during the await
			const current = sessions.find((s) => s.sessionId === sessionId);
			if (!current || current.dead) return;

			// Mark attached, subscribe to live output
			if (!current.attached) {
				subscribeSession(sessionId);
				sessions = sessions.map((s) =>
					s.sessionId === sessionId ? { ...s, attached: true } : s
				);
				await tick();
				syncTerminalSize(sessionId, true);
			}

			// Replay buffered entries
			if (result.entries && result.entries.length > 0) {
				const ref = getTermRef(sessionId);
				if (ref) {
					suppressInputForReplay(sessionId);
					let maxSeq = since;
					for (const entry of result.entries) {
						ref.write(entry.data);
						if (entry.seq > maxSeq) maxSeq = entry.seq;
					}
					updateSessionSeq(sessionId, maxSeq);
				} else {
					// Terminal not mounted yet — buffer for later
					let buf = outputBuffer.get(sessionId);
					if (!buf) { buf = []; outputBuffer.set(sessionId, buf); }
					let maxSeq = since;
					for (const entry of result.entries) {
						buf.push(entry.data);
						if (entry.seq > maxSeq) maxSeq = entry.seq;
					}
					updateSessionSeq(sessionId, maxSeq);
					suppressOnFlush.add(sessionId);
					tick().then(() => flushBuffer(sessionId));
				}
			}
		} catch (err) {
			console.error('Failed to attach session:', err);
			// Attach failed — mark dead so the user sees the failure
			unsubscribeSession(sessionId);
			sessions = sessions.map((s) =>
				s.sessionId === sessionId
					? { ...s, dead: true, attached: false, aiIsWorking: false, aiActivity: undefined, aiStatusMessage: undefined }
					: s
			);
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

	/** Kill a session by its server sessionId (works even without a local tab). */
	export async function killSessionById(sessionId: string): Promise<void> {
		try {
			await client.killSession(sessionId);
		} catch {
			// Session may already be dead
		}
		removeSession(sessionId);
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

	const renameTimers = new Map<string, ReturnType<typeof setTimeout>>();

	export function renameSession(key: string, label: string): void {
		const sid = sessionIdFor(key);
		if (!sid) return;
		// Update local state immediately for responsive UI
		sessions = sessions.map((s) =>
			s.key === key ? { ...s, label } : s
		);
		// Debounce the server call by 500ms
		const existing = renameTimers.get(key);
		if (existing) clearTimeout(existing);
		renameTimers.set(key, setTimeout(() => {
			renameTimers.delete(key);
			client.renameSession(sid, label).catch(() => {});
		}, 500));
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

		// Handle split group cleanup before removing
		if (key) {
			const found = groupForKey(key);
			if (found) {
				// If viewing this group, promote the surviving pane
				if (activeKey === found.primaryKey && key === found.primaryKey) {
					activeKey = found.group.secondaryKey;
				}
				const { [found.primaryKey]: _, ...remaining } = splitGroups;
				splitGroups = remaining;
				focusedPane = 'primary';
			}
		}

		sessions = sessions.filter((s) => s.sessionId !== sessionId);
		if (activeKey === key) {
			activeKey = sessions.length > 0 ? sessions[sessions.length - 1].key : null;
		}
		config.callbacks?.onSessionClosed?.(sessionId, 'closed');

		// Re-fit after split collapse
		if (!splitDirection) {
			fitAfterLayout();
		}
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
		const session = focusedSession();
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

	/** Set AI permission for all attached sessions at once (master toggle). */
	export async function setAllAi(allowed: boolean): Promise<void> {
		const attached = sessions.filter((s) => s.attached && !s.dead);
		await Promise.allSettled(
			attached.map(async (s) => {
				try {
					await client.setUserAllowsAi(s.sessionId, allowed);
				} catch {}
			})
		);
		sessions = sessions.map((s) =>
			s.attached && !s.dead ? { ...s, userAllowsAi: allowed } : s
		);
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
		// Only update the ControlBar dims display from the focused pane
		const focused = focusedSession();
		if (!focused || focused.sessionId === sessionId) {
			terminalRows = rows;
			terminalCols = cols;
		}
		if (connectionStatus === 'connected') {
			client.resizeSession(sessionId, rows, cols).catch(() => {});
		}
		config.callbacks?.onResize?.(sessionId, rows, cols);
	}

	async function handleSignal(signal: number): Promise<void> {
		const session = focusedSession();
		if (!session) return;
		try {
			await client.sendSignal(session.sessionId, signal);
		} catch (err) {
			console.error('Failed to send signal:', err);
		}
	}

	// ── Search ──────────────────────────────────────────────────────

	export function toggleSearch(): void {
		searchVisible = !searchVisible;
	}

	// ── Split panes ────────────────────────────────────────────────

	export function splitHorizontal(): void {
		const g = currentGroup();
		if (g) {
			// Viewing a split — toggle direction or unsplit
			if (g.direction === 'horizontal') { unsplit(); return; }
			splitGroups = { ...splitGroups, [activeKey!]: { ...g, direction: 'horizontal' } };
			fitAfterLayout();
			return;
		}
		// Not in a group — open picker (existing groups stay intact)
		splitPickerDirection = splitPickerDirection === 'horizontal' ? null : 'horizontal';
	}

	export function splitVertical(): void {
		const g = currentGroup();
		if (g) {
			if (g.direction === 'vertical') { unsplit(); return; }
			splitGroups = { ...splitGroups, [activeKey!]: { ...g, direction: 'vertical' } };
			fitAfterLayout();
			return;
		}
		splitPickerDirection = splitPickerDirection === 'vertical' ? null : 'vertical';
	}

	export function unsplit(): void {
		if (!activeKey) return;
		const g = splitGroups[activeKey];
		if (!g) return;
		const primaryKey = activeKey;
		// Stay on the focused pane
		if (focusedPane === 'secondary') {
			activeKey = g.secondaryKey;
		}
		// Remove this group
		const { [primaryKey]: _, ...remaining } = splitGroups;
		splitGroups = remaining;
		focusedPane = 'primary';
		fitAfterLayout();
	}

	function doSplit(dir: 'horizontal' | 'vertical', targetKey?: string): void {
		splitPickerDirection = null;
		if (!activeKey || sessions.length < 1) return;

		if (targetKey) {
			// Existing session — immediate split, focus stays on primary
			splitGroups = { ...splitGroups, [activeKey]: { secondaryKey: targetKey, direction: dir, ratio: 0.5 } };
			focusedPane = 'primary';
			fitAfterLayout();
		} else {
			// New session — cursor will land in the new (secondary) pane
			const primaryKey = activeKey;
			splitSetupInProgress = true;
			startSession().then(() => {
				const newKey = sessions[sessions.length - 1]?.key;
				if (newKey) {
					splitGroups = { ...splitGroups, [primaryKey]: { secondaryKey: newKey, direction: dir, ratio: 0.5 } };
				}
				activeKey = primaryKey;
				focusedPane = 'secondary';
				splitSetupInProgress = false;
				fitAfterLayout();
			}).catch(() => {
				splitSetupInProgress = false;
			});
		}
	}

	function dismissPicker(): void {
		splitPickerDirection = null;
	}

	function fitGroupPanes(primaryKey: string): void {
		const g = splitGroups[primaryKey];
		if (!g) return;
		terminalRefs[primaryKey]?.fit();
		terminalRefs[g.secondaryKey]?.fit();
	}

	function fitAfterLayout(): void {
		tick().then(() => requestAnimationFrame(() => {
			if (activeKey && splitGroups[activeKey]) {
				fitGroupPanes(activeKey);
			} else if (activeKey) {
				terminalRefs[activeKey]?.fit();
			}
		}));
	}

	function handlePaneFocus(pane: 'primary' | 'secondary'): void {
		focusedPane = pane;
		const g = currentGroup();
		const key = pane === 'secondary' ? g?.secondaryKey : activeKey;
		if (key) {
			const ref = terminalRefs[key];
			if (ref) {
				const size = ref.getSize();
				if (size) {
					terminalRows = size.rows;
					terminalCols = size.cols;
				}
			}
		}
	}

	function handlePaneClickFocus(key: string): void {
		if (!isSplitVisible()) return;
		const g = currentGroup()!;
		if (key === activeKey) handlePaneFocus('primary');
		else if (key === g.secondaryKey) handlePaneFocus('secondary');
	}

	function isSessionFocused(key: string): boolean {
		if (isSplitVisible()) {
			const g = currentGroup()!;
			return (focusedPane === 'primary' && key === activeKey) ||
				(focusedPane === 'secondary' && key === g.secondaryKey);
		}
		return key === activeKey;
	}

	function getFocusBorder(key: string): string {
		if (!isSplitVisible()) return '2px solid transparent';
		return isSessionFocused(key) ? '2px solid #3b82f6' : '2px solid transparent';
	}

	function getTerminalLayout(key: string): { visible: boolean; style: string; zIndex: number } {
		const g = currentGroup();
		if (!g) {
			return { visible: key === activeKey, style: 'top:0;left:0;right:0;bottom:0;', zIndex: key === activeKey ? 1 : 0 };
		}
		const pct = g.ratio * 100;
		const gap = 2;
		if (g.direction === 'vertical') {
			if (key === activeKey) return { visible: true, style: `top:0;bottom:0;left:0;width:calc(${pct}% - ${gap}px);`, zIndex: 1 };
			if (key === g.secondaryKey) return { visible: true, style: `top:0;bottom:0;left:calc(${pct}% + ${gap}px);right:0;`, zIndex: 1 };
		} else {
			if (key === activeKey) return { visible: true, style: `left:0;right:0;top:0;height:calc(${pct}% - ${gap}px);`, zIndex: 1 };
			if (key === g.secondaryKey) return { visible: true, style: `left:0;right:0;top:calc(${pct}% + ${gap}px);bottom:0;`, zIndex: 1 };
		}
		return { visible: false, style: 'top:0;left:0;right:0;bottom:0;', zIndex: 0 };
	}

	function getDividerStyle(): string {
		const g = currentGroup();
		if (!g) return '';
		const pct = g.ratio * 100;
		if (g.direction === 'vertical') {
			return `top:0;bottom:0;left:calc(${pct}% - 2px);width:4px;`;
		}
		return `left:0;right:0;top:calc(${pct}% - 2px);height:4px;`;
	}

	function handleDividerMouseDown(e: MouseEvent): void {
		e.preventDefault();
		draggingSplit = true;
		document.body.style.userSelect = 'none';
		const pk = activeKey!;

		const onMouseMove = (ev: MouseEvent) => {
			if (!terminalAreaEl) return;
			const g = splitGroups[pk];
			if (!g) return;
			const rect = terminalAreaEl.getBoundingClientRect();
			let newRatio: number;
			if (g.direction === 'vertical') {
				newRatio = (ev.clientX - rect.left) / rect.width;
			} else {
				newRatio = (ev.clientY - rect.top) / rect.height;
			}
			const totalSize = g.direction === 'vertical' ? rect.width : rect.height;
			const minRatio = 200 / totalSize;
			const maxRatio = 1 - minRatio;
			splitGroups = { ...splitGroups, [pk]: { ...g, ratio: Math.max(minRatio, Math.min(maxRatio, newRatio)) } };
		};

		const cleanup = () => {
			draggingSplit = false;
			document.body.style.userSelect = '';
			window.removeEventListener('mousemove', onMouseMove);
			window.removeEventListener('mouseup', cleanup);
			dragCleanup = undefined;
			fitAfterLayout();
		};

		dragCleanup = cleanup;
		window.addEventListener('mousemove', onMouseMove);
		window.addEventListener('mouseup', cleanup);
	}

	export function toggleSplitFocus(): void {
		const g = currentGroup();
		if (!g) return;
		focusedPane = focusedPane === 'primary' ? 'secondary' : 'primary';
		handlePaneFocus(focusedPane);
		const key = focusedPane === 'secondary' ? g.secondaryKey : activeKey;
		if (key) terminalRefs[key]?.focus();
	}

	export function getSplitSecondaryKey(): string | null {
		return currentGroup()?.secondaryKey ?? null;
	}

	export function getSplitPrimaryKey(): string | null {
		return isSplitVisible() ? activeKey : null;
	}

	/** Get all split groups for parent components (tabs, sidebar). */
	export function getSplitGroups(): Array<{ primaryKey: string; secondaryKey: string; direction: 'horizontal' | 'vertical' }> {
		return Object.entries(splitGroups).map(([pk, g]) => ({
			primaryKey: pk,
			secondaryKey: g.secondaryKey,
			direction: g.direction
		}));
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

	/** Send a command to the focused PTY session (e.g. `cd /path`). */
	export function execInActiveSession(command: string): void {
		const sess = focusedSession();
		if (!sess) return;
		client.execCommand(sess.sessionId, command);
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
		const preCreated = !!config.client;
		client = config.client ?? new SctlWsClient(config.wsUrl, config.apiKey, config.reconnect);

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

		client.on('activity.new', (msg: WsActivityNewMsg) => {
			config.callbacks?.onActivity?.(msg.entry);
		});

		if (preCreated) {
			// Client was pre-created — if already connected, fire connected logic
			if (client.status === 'connected') {
				connectionStatus = 'connected';
				config.callbacks?.onConnectionChange?.('connected');
				fetchRemoteSessions();
			}
		} else if (config.autoConnect !== false) {
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

	$effect(() => {
		// Report all split groups to parent
		const groups = Object.entries(splitGroups).map(([pk, g]) => ({
			primaryKey: pk,
			secondaryKey: g.secondaryKey,
			direction: g.direction
		}));
		untrack(() => config.callbacks?.onSplitGroupsChange?.(groups));
	});

	$effect(() => {
		const pane = focusedPane;
		untrack(() => config.callbacks?.onFocusedPaneChange?.(pane));
	});

	$effect(() => {
		return () => { dragCleanup?.(); };
	});

</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="sctlin-container flex flex-col h-full text-neutral-200"
	onkeydown={(e) => { if (e.key === 'Escape' && splitPickerDirection) { e.stopPropagation(); dismissPicker(); } }}>
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
	<div class="flex-1 relative min-h-0"
		 bind:this={terminalAreaEl}
		 style:margin-right="{rightInset ?? 0}px"
		 style:transition={rightInsetAnimate ? 'margin 300ms ease-in-out' : 'none'}>
		{#if sessions.length > 0}
			<div class="absolute inset-0 pointer-events-none" style="background: rgba(12, 12, 12, 0.85); z-index: 0;"></div>
		{/if}

		{#each sessions as session (session.key)}
			{@const layout = getTerminalLayout(session.key)}
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<!-- svelte-ignore a11y_click_events_have_key_events -->
			<div
				class="absolute overflow-hidden"
				style="{layout.style}"
				style:z-index={layout.zIndex}
				style:visibility={layout.visible ? 'inherit' : 'hidden'}
				style:pointer-events={draggingSplit ? 'none' : 'auto'}
			>
				<div
					class="w-full h-full"
					style:border-top={getFocusBorder(session.key)}
					onclick={() => handlePaneClickFocus(session.key)}
				>
					<!-- svelte-ignore binding_property_non_reactive -->
					<Terminal
						theme={config.theme}
						readonly={session.dead || session.aiIsWorking || !session.attached}
						overlayLabel={session.dead ? 'Session Lost' : session.aiIsWorking ? (session.aiActivity === 'read' ? 'AI Reading' : 'AI Executing') : undefined}
						overlayColor={session.dead ? 'gray' : session.aiActivity === 'read' ? 'blue' : 'green'}
						showSearch={searchVisible && isSessionFocused(session.key)}
						rows={config.defaultRows}
						cols={config.defaultCols}
						ondata={(data) => handleTerminalData(session.sessionId, data)}
						onresize={(r, c) => handleTerminalResize(session.sessionId, r, c)}
						onready={() => {
							flushBuffer(session.sessionId);
							syncTerminalSize(session.sessionId, session.attached);
						}}
						onsearchclose={() => { searchVisible = false; }}
						bind:this={terminalRefs[session.key]}
					/>
				</div>
			</div>
		{/each}

		{#if isSplitVisible()}
			{@const g = currentGroup()!}
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<div
				class="absolute transition-colors {g.direction === 'vertical' ? 'cursor-col-resize' : 'cursor-row-resize'} {draggingSplit ? 'bg-neutral-500' : 'bg-neutral-700 hover:bg-neutral-500'}"
				style="{getDividerStyle()}"
				style:z-index="2"
				onmousedown={handleDividerMouseDown}
			></div>
		{/if}
	</div>

	<!-- Split session picker -->
	{#if splitPickerDirection}
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<!-- svelte-ignore a11y_click_events_have_key_events -->
		<div class="fixed inset-0 z-20" onclick={dismissPicker}></div>
		<div class="absolute bottom-8 left-1 z-30 bg-neutral-800 border border-neutral-700 rounded shadow-xl text-xs min-w-[180px]">
			{#each sessions.filter(s => s.key !== activeKey && !s.dead && !allGroupedKeys().has(s.key)) as s (s.key)}
				<button
					class="flex items-center gap-2 w-full px-3 py-1.5 hover:bg-neutral-700 transition-colors text-left"
					onclick={() => doSplit(splitPickerDirection!, s.key)}
				>
					<span class="w-1.5 h-1.5 rounded-full shrink-0
						{s.attached ? 'bg-green-500' : 'bg-yellow-500'}"></span>
					<span class="font-mono text-neutral-300 truncate">{s.label || s.sessionId.slice(0, 12)}</span>
				</button>
			{/each}
			{#if sessions.filter(s => s.key !== activeKey && !s.dead && !allGroupedKeys().has(s.key)).length > 0}
				<div class="border-t border-neutral-700"></div>
			{/if}
			<button
				class="flex items-center gap-2 w-full px-3 py-1.5 hover:bg-neutral-700 transition-colors text-left text-neutral-400"
				onclick={() => doSplit(splitPickerDirection!)}
			>
				<svg class="w-3 h-3 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" />
				</svg>
				<span class="font-mono">New session</span>
			</button>
		</div>
	{/if}

	<!-- Control bar — shown whenever a session tab is open -->
	{#if activeSession()}
		{@const focused = focusedSession()}
		<ControlBar
			userAllowsAi={focused?.userAllowsAi ?? false}
			aiIsWorking={focused?.aiIsWorking ?? false}
			aiActivity={focused?.aiActivity}
			aiStatusMessage={focused?.aiStatusMessage}
			disabled={!focused?.attached || focused?.dead === true}
			{terminalRows}
			{terminalCols}
			ontoggleai={toggleUserAllowsAi}
			onsignal={handleSignal}
			{onToggleFiles}
			{onTogglePlaybooks}
			{sidePanelOpen}
			{sidePanelTab}
			splitDirection={currentGroup()?.direction ?? null}
			onsplithorizontal={splitHorizontal}
			onsplitvertical={splitVertical}
		/>
	{/if}
</div>

<style>
	.sctlin-container {
		min-height: 200px;
	}
</style>
