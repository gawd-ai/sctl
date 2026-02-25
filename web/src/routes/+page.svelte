<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { TerminalContainer, TerminalTabs, ServerPanel, ToastContainer, QuickExecBar, FileBrowser, CommandPalette, KeyboardShortcuts, TransferIndicator, PlaybookPanel, SidePanel, ServerDashboard } from '$lib';
	import ExecViewer from '$lib/components/ExecViewer.svelte';
	import { ConnectionManager } from '$lib/utils/connection-manager';
	import { KeyboardManager } from '$lib/utils/keyboard';
	import { AppSidebar } from 'gawdux';
	import type { SidebarConfig } from 'gawdux';
	import type { ClientTransfer } from '$lib/utils/transfer';
	import type {
		SctlinConfig,
		SessionInfo,
		ConnectionStatus,
		RemoteSessionInfo,
		ServerConfig,
		DeviceInfo,
		ActivityEntry,
		SplitGroupInfo,
		ViewerTab,
		ExecViewerData
	} from '$lib';

	// ── Persistence ──────────────────────────────────────────────────

	const STORAGE_KEY = 'sctlin-servers';
	const OLD_STORAGE_KEY = 'sctlin-dev-settings';

	interface ServerEntry extends ServerConfig {
		connected: boolean;
	}

	const DEFAULT_SERVER: ServerEntry = {
		id: 'local',
		name: 'local',
		wsUrl: 'ws://localhost:1337/api/ws',
		apiKey: 'dev-key',
		shell: '',
		connected: false
	};

	function loadServers(): ServerEntry[] {
		let entries: ServerEntry[] | null = null;

		try {
			const raw = localStorage.getItem(STORAGE_KEY);
			if (raw) {
				const parsed = JSON.parse(raw);
				if (Array.isArray(parsed) && parsed.length > 0) entries = parsed;
			}
		} catch {}

		// Migrate from old format
		if (!entries) {
			try {
				const old = localStorage.getItem(OLD_STORAGE_KEY);
				if (old) {
					const parsed = JSON.parse(old);
					const migrated: ServerEntry = {
						id: 'local',
						name: 'local',
						wsUrl: parsed.wsUrl || DEFAULT_SERVER.wsUrl,
						apiKey: parsed.apiKey || DEFAULT_SERVER.apiKey,
						shell: parsed.shell || '',
						connected: false
					};
					try {
						if (sessionStorage.getItem('sctlin-connected') === 'true') {
							migrated.connected = true;
						}
					} catch {}
					localStorage.removeItem(OLD_STORAGE_KEY);
					try { sessionStorage.removeItem('sctlin-connected'); } catch {}
					entries = [migrated];
				}
			} catch {}
		}

		return entries ?? [DEFAULT_SERVER];
	}

	/**
	 * Merge seed servers from sctlin-seed.json into the server list.
	 * Matches by `id` — existing entries are updated (name, wsUrl, apiKey, shell),
	 * new entries are appended. Seed servers that were removed by the user
	 * (tracked via `removedSeeds`) are not re-added.
	 */
	const REMOVED_SEEDS_KEY = 'sctlin-removed-seeds';

	async function mergeSeedServers(): Promise<void> {
		try {
			const resp = await fetch('/sctlin-seed.json');
			if (!resp.ok) return;
			const seed: ServerEntry[] = await resp.json();
			if (!Array.isArray(seed) || seed.length === 0) return;

			// Load set of seed IDs the user explicitly removed
			let removedSeeds: Set<string>;
			try {
				const raw = localStorage.getItem(REMOVED_SEEDS_KEY);
				removedSeeds = raw ? new Set(JSON.parse(raw)) : new Set();
			} catch { removedSeeds = new Set(); }

			let changed = false;
			const existingIds = new Set(servers.map((s) => s.id));

			for (const entry of seed) {
				if (removedSeeds.has(entry.id)) continue;

				if (existingIds.has(entry.id)) {
					// Update existing entry's config (but don't touch connected state)
					const idx = servers.findIndex((s) => s.id === entry.id);
					if (idx !== -1) {
						const old = servers[idx];
						if (old.name !== entry.name || old.wsUrl !== entry.wsUrl ||
							old.apiKey !== entry.apiKey || old.shell !== entry.shell) {
							servers[idx] = { ...old, ...entry, id: old.id };
							changed = true;
						}
					}
				} else {
					// New seed server — append
					servers = [...servers, { ...entry, connected: false } as ServerConfig];
					changed = true;
				}
			}

			if (changed) saveServers();
		} catch {}
	}

	function saveServers(): void {
		try {
			const entries: ServerEntry[] = servers.map((s) => ({
				...s,
				connected: !!serverConfigs[s.id]
			}));
			localStorage.setItem(STORAGE_KEY, JSON.stringify(entries));
		} catch {}
	}

	// ── State ────────────────────────────────────────────────────────

	let servers: ServerConfig[] = $state([]);
	let connectionStatuses: Record<string, ConnectionStatus> = $state({});
	let serverSessions: Record<string, SessionInfo[]> = $state({});
	let serverRemoteSessions: Record<string, RemoteSessionInfo[]> = $state({});
	let serverConfigs: Record<string, SctlinConfig> = $state({});
	// Plain object — NOT $state. Using $state({}) with bind:this inside {#each}
	// loses refs when the array/object driving the loop is reassigned.
	let containerRefs: Record<string, TerminalContainer | undefined> = {};
	let activeServerId: string | null = $state(null);
	let activeKeys: Record<string, string | null> = $state({});

	// Device info
	let serverDeviceInfo: Record<string, DeviceInfo | null> = $state({});

	// Transfer lists (per-server, updated via manager events)
	let serverTransferLists: Record<string, ClientTransfer[]> = $state({});

	// Split groups (per-server, for tab/sidebar highlighting)
	let serverSplitGroups: Record<string, SplitGroupInfo[]> = $state({});
	let focusedPanes: Record<string, 'primary' | 'secondary'> = $state({});

	// Activity feed
	let serverActivity: Record<string, ActivityEntry[]> = $state({});

	// Server dashboard
	let serverDashboardActive: Record<string, boolean> = $state({});
	let dashboardActive = $derived(activeServerId ? serverDashboardActive[activeServerId] ?? false : false);

	// Viewer tabs (exec results, files, etc.)
	let serverViewerTabs: Record<string, ViewerTab[]> = $state({});
	let activeViewerKey: Record<string, string | null> = $state({});
	let viewerActive = $derived(
		activeServerId ? !!(activeViewerKey[activeServerId]) : false
	);

	// Master AI toggle (per server)
	let serverMasterAi: Record<string, boolean> = $state({});

	// Toast
	let toastRef: ToastContainer | undefined = $state();

	// Keyboard
	const keyboard = new KeyboardManager();

	// Connection manager (framework-agnostic orchestrator)
	const manager = new ConnectionManager(
		{ maxActivityEntries: 200 },
		{
			onConnectionChange: (id, status) => {
				const prevStatus = connectionStatuses[id];
				connectionStatuses = { ...connectionStatuses, [id]: status };
				if (status === 'connected' && prevStatus !== 'connected') {
					toastRef?.push(`Connected to ${servers.find(s => s.id === id)?.name ?? id}`, 'success');
				} else if (status === 'reconnecting' && prevStatus === 'connected') {
					toastRef?.push(`Reconnecting to ${servers.find(s => s.id === id)?.name ?? id}...`, 'warning');
				} else if (status === 'disconnected' && prevStatus === 'connected') {
					toastRef?.push(`Disconnected from ${servers.find(s => s.id === id)?.name ?? id}`, 'info');
				}
			},
			onDeviceInfo: (id, info) => {
				serverDeviceInfo = { ...serverDeviceInfo, [id]: info };
			},
			onActivity: (id, entries) => {
				serverActivity = { ...serverActivity, [id]: entries };
			},
			onTransferChange: (id, transfers) => {
				serverTransferLists = { ...serverTransferLists, [id]: transfers };
			},
			onTransferError: (_id, _ct, msg) => {
				toastRef?.push(msg, 'error');
			},
			onError: (_id, err) => {
				console.error('[sctlin]', err.message);
			}
		}
	);

	// Quick exec
	let quickExecVisible = $state(false);

	// Side panel: open/closed is per-server, active tab is per-session
	let serverPanelOpen: Record<string, boolean> = $state({});
	let sessionPanelTab: Record<string, string> = $state({});
	const PANEL_WIDTH_KEY = 'sctlin-panel-width';
	let panelWidth = $state(
		(() => {
			try {
				// Try new key first, then fall back to old filebrowser key for migration
				const v = localStorage.getItem(PANEL_WIDTH_KEY) ?? localStorage.getItem('sctlin-filebrowser-width');
				return parseInt(v ?? '') || 384;
			} catch { return 384; }
		})()
	);
	let panelAnimating = $state(false);
	let panelResizing = $state(false);

	// Persist shared width
	$effect(() => { try { localStorage.setItem(PANEL_WIDTH_KEY, String(panelWidth)); } catch {} });

	// Command palette
	let commandPaletteVisible = $state(false);

	// Keyboard shortcuts panel (shown in sidebar)
	let shortcutsVisible = $state(false);
	let sidebarToggleFn: (() => void) | null = null;
	let shortcutsAutoExpanded = false; // true if Alt+/ auto-expanded the sidebar

	// Reset button two-click confirmation
	let resetConfirming = $state(false);

	const sidebarConfig: SidebarConfig = {
		storageKey: 'sctlin-sidebar',
		defaultCollapsed: true,
		expandedWidth: 256,
		collapsedWidth: 36,
		showToggleAll: false
	};

	// ── Derived ──────────────────────────────────────────────────────

	let hasAnySessions = $derived(
		Object.values(serverSessions).some((list) => list.length > 0)
	);

	// #12: Active server display info
	let activeServer = $derived(
		activeServerId ? servers.find((s) => s.id === activeServerId) ?? null : null
	);

	let activeServerStatus = $derived(
		activeServerId ? connectionStatuses[activeServerId] ?? 'disconnected' : null
	);

	/** Active session key for the active server. */
	let activeSessionKey = $derived(
		activeServerId ? activeKeys[activeServerId] ?? null : null
	);

	let activeTransferList = $derived(
		activeServerId ? serverTransferLists[activeServerId] ?? [] : []
	);

	let activeTransferTracker = $derived(
		activeServerId ? manager.get(activeServerId)?.transferTracker ?? null : null
	);

	let activeSplitGroups = $derived(
		activeServerId ? serverSplitGroups[activeServerId] ?? [] : []
	);

	let activeFocusedPane = $derived(
		activeServerId ? focusedPanes[activeServerId] ?? 'primary' : 'primary' as const
	);

	/** The session key that has focus — accounts for split pane focus. */
	function focusedKeyFor(serverId: string): string | null {
		const ak = activeKeys[serverId];
		if (!ak) return null;
		const groups = serverSplitGroups[serverId] ?? [];
		const group = groups.find(g => g.primaryKey === ak);
		if (group && (focusedPanes[serverId] ?? 'primary') === 'secondary') {
			return group.secondaryKey;
		}
		return ak;
	}

	let focusedSessionKey = $derived(
		activeServerId ? focusedKeyFor(activeServerId) : null
	);

	let activePanelOpen = $derived(
		activeServerId ? (serverPanelOpen[activeServerId] ?? false) : false
	);
	let activePanelTab = $derived(
		focusedSessionKey ? (sessionPanelTab[focusedSessionKey] ?? 'files') : 'files'
	);

	/** Key for dashboard-level side panel state (distinct from session keys). */
	function dashPanelKey(serverId: string): string { return '_dash_' + serverId; }

	function toggleSidePanel(tabId: string, sessionKey?: string) {
		const serverId = activeServerId;
		if (!serverId) return;
		const key = sessionKey ?? focusedSessionKey;
		if (!key) return;
		shortcutsVisible = false;

		const isOpen = serverPanelOpen[serverId] ?? false;
		const currentTab = sessionPanelTab[key] ?? 'files';

		if (isOpen && currentTab === tabId) {
			// Same tab — close panel
			panelAnimating = true;
			serverPanelOpen = { ...serverPanelOpen, [serverId]: false };
			setTimeout(() => { panelAnimating = false; }, 350);
		} else if (isOpen) {
			// Different tab — switch instantly
			sessionPanelTab = { ...sessionPanelTab, [key]: tabId };
		} else {
			// Closed — open with animation
			panelAnimating = true;
			serverPanelOpen = { ...serverPanelOpen, [serverId]: true };
			sessionPanelTab = { ...sessionPanelTab, [key]: tabId };
			setTimeout(() => { panelAnimating = false; }, 350);
		}
	}

	function closeSidePanel() {
		const serverId = activeServerId;
		if (!serverId) return;
		if (!(serverPanelOpen[serverId] ?? false)) return;
		panelAnimating = true;
		serverPanelOpen = { ...serverPanelOpen, [serverId]: false };
		setTimeout(() => { panelAnimating = false; }, 350);
	}

	// ── Viewer tab management ───────────────────────────────────────

	function openViewerTab(serverId: string, tab: ViewerTab): void {
		const tabs = serverViewerTabs[serverId] ?? [];
		// Deduplicate: if a viewer for the same content exists, just focus it
		const existing = tabs.find(t => {
			if (t.type !== tab.type) return false;
			if (t.type === 'exec' && tab.type === 'exec') return (t.data as ExecViewerData).activityId === (tab.data as ExecViewerData).activityId;
			return t.label === tab.label;
		});
		if (existing) {
			selectViewerTab(serverId, existing.key);
			return;
		}
		serverViewerTabs = { ...serverViewerTabs, [serverId]: [...tabs, tab] };
		activeViewerKey = { ...activeViewerKey, [serverId]: tab.key };
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
	}

	function closeViewerTab(serverId: string, key: string): void {
		const tabs = (serverViewerTabs[serverId] ?? []).filter(t => t.key !== key);
		serverViewerTabs = { ...serverViewerTabs, [serverId]: tabs };
		if (activeViewerKey[serverId] === key) {
			activeViewerKey = { ...activeViewerKey, [serverId]: null };
		}
	}

	function selectViewerTab(serverId: string, key: string): void {
		activeServerId = serverId;
		activeViewerKey = { ...activeViewerKey, [serverId]: key };
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
	}

	function getActiveSessionId(): string | null {
		if (!activeServerId) return null;
		const key = activeKeys[activeServerId];
		if (!key) return null;
		return serverSessions[activeServerId]?.find((s) => s.key === key)?.sessionId ?? null;
	}

	// Connected servers in display order
	let connectedServers = $derived(
		servers.filter((s) => connectionStatuses[s.id] === 'connected')
	);

	// Unified flat session list across all servers (for tab navigation)
	let unifiedSessions = $derived(
		connectedServers.flatMap((server) => {
			const list = serverSessions[server.id] ?? [];
			return list.map((s) => ({
				...s,
				serverId: server.id,
				serverName: connectedServers.length > 1 ? server.name : undefined
			}));
		})
	);

	// Which session key is active in the unified bar
	let unifiedActiveKey = $derived(
		activeServerId ? activeKeys[activeServerId] ?? null : null
	);

	// ── Master AI toggle ────────────────────────────────────────────

	async function toggleMasterAi(serverId: string): Promise<void> {
		const current = serverMasterAi[serverId] ?? false;
		const newVal = !current;
		serverMasterAi = { ...serverMasterAi, [serverId]: newVal };
		const ref = containerRefs[serverId];
		if (ref) await ref.setAllAi(newVal);
	}

	// ── Server management ────────────────────────────────────────────

	function connectServer(id: string): void {
		const server = servers.find((s) => s.id === id);
		if (!server || serverConfigs[id]) return;

		const conn = manager.connect(server);
		const cfg = manager.buildSctlinConfig(server, {
			onRemoteSessions: (sessions) => {
				serverRemoteSessions = { ...serverRemoteSessions, [id]: sessions };
			},
			onSessionsChange: (sessions) => {
				serverSessions = { ...serverSessions, [id]: sessions };
			},
			onActiveSessionChange: (key) => {
				activeKeys = { ...activeKeys, [id]: key };
			},
			onSplitGroupsChange: (groups) => {
				serverSplitGroups = { ...serverSplitGroups, [id]: groups };
			},
			onFocusedPaneChange: (pane) => {
				focusedPanes = { ...focusedPanes, [id]: pane };
			},
			onError: (err) => {
				console.error(`[sctlin:${server.name}]`, err.message);
				toastRef?.push(err.message, 'error');
			}
		});

		serverConfigs = { ...serverConfigs, [id]: cfg };
		connectionStatuses = { ...connectionStatuses, [id]: conn.status };
		activeServerId = id;
		saveServers();
	}

	function disconnectServer(id: string): void {
		manager.disconnect(id);
		const { [id]: _, ...rest } = serverConfigs;
		serverConfigs = rest;
		connectionStatuses = { ...connectionStatuses, [id]: 'disconnected' };
		serverSessions = { ...serverSessions, [id]: [] };
		serverRemoteSessions = { ...serverRemoteSessions, [id]: [] };
		delete containerRefs[id];
		const { [id]: _tl, ...restTl } = serverTransferLists;
		serverTransferLists = restTl;
		const { [id]: _di, ...restDi } = serverDeviceInfo;
		serverDeviceInfo = restDi;
		const { [id]: _act, ...restAct } = serverActivity;
		serverActivity = restAct;
		const { [id]: _sg, ...restSg } = serverSplitGroups;
		serverSplitGroups = restSg;
		const { [id]: _fp, ...restFp } = focusedPanes;
		focusedPanes = restFp;
		const { [id]: _vt, ...restVt } = serverViewerTabs;
		serverViewerTabs = restVt;
		const { [id]: _vk, ...restVk } = activeViewerKey;
		activeViewerKey = restVk;
		if (activeServerId === id) {
			const connected = Object.keys(rest);
			activeServerId = connected.length > 0 ? connected[0] : null;
		}
		saveServers();
	}

	function addServer(partial: Omit<ServerConfig, 'id'>): void {
		const server: ServerConfig = {
			id: crypto.randomUUID(),
			...partial
		};
		servers = [...servers, server];
		saveServers();
	}

	function removeServer(id: string): void {
		if (serverConfigs[id]) disconnectServer(id);
		servers = servers.filter((s) => s.id !== id);
		const { [id]: _cs, ...restCs } = connectionStatuses;
		connectionStatuses = restCs;
		const { [id]: _ss, ...restSs } = serverSessions;
		serverSessions = restSs;
		const { [id]: _rs, ...restRs } = serverRemoteSessions;
		serverRemoteSessions = restRs;
		const { [id]: _as, ...restAs } = activeKeys;
		activeKeys = restAs;
		// Track removed seed servers so they don't reappear
		try {
			const raw = localStorage.getItem(REMOVED_SEEDS_KEY);
			const removed: string[] = raw ? JSON.parse(raw) : [];
			if (!removed.includes(id)) {
				removed.push(id);
				localStorage.setItem(REMOVED_SEEDS_KEY, JSON.stringify(removed));
			}
		} catch {}
		saveServers();
	}

	// #9: Fix editServer — compare old vs new before reconnecting
	function editServer(id: string, updates: Partial<ServerConfig>): void {
		const old = servers.find((s) => s.id === id);
		servers = servers.map((s) => (s.id === id ? { ...s, ...updates } : s));

		if (serverConfigs[id] && old) {
			const wsChanged = updates.wsUrl !== undefined && updates.wsUrl !== old.wsUrl;
			const apiKeyChanged = updates.apiKey !== undefined && updates.apiKey !== old.apiKey;

			if (wsChanged || apiKeyChanged) {
				// Connection params changed — reconnect
				disconnectServer(id);
				connectServer(id);
			} else if (updates.shell !== undefined && updates.shell !== old.shell) {
				// Shell-only change — update sessionDefaults in existing config
				const existing = serverConfigs[id];
				serverConfigs = {
					...serverConfigs,
					[id]: {
						...existing,
						sessionDefaults: {
							...existing.sessionDefaults,
							shell: updates.shell || undefined
						}
					}
				};
			}
		}
		saveServers();
	}

	// ── Session actions ──────────────────────────────────────────────

	/** Wait for a container ref and connected status, then execute an action. */
	function withContainer(serverId: string, action: (ref: TerminalContainer) => void): void {
		activeServerId = serverId;
		// If container is ready and connected, execute immediately
		const ref = containerRefs[serverId];
		if (ref && connectionStatuses[serverId] === 'connected') {
			action(ref);
			return;
		}
		// Server not connected — connect first
		if (!serverConfigs[serverId]) {
			connectServer(serverId);
		}
		// Wait for connection + container to be ready
		const unsub = $effect.root(() => {
			$effect(() => {
				if (connectionStatuses[serverId] === 'connected' && containerRefs[serverId]) {
					unsub();
					action(containerRefs[serverId]!);
				}
			});
		});
		setTimeout(() => { try { unsub(); } catch {} }, 15000);
	}

	function selectSession(serverId: string, sessionId: string): void {
		activeServerId = serverId;
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
		const key = serverSessions[serverId]?.find((s) => s.sessionId === sessionId)?.key;
		if (key) containerRefs[serverId]?.selectSession(key);
	}

	function attachSession(serverId: string, sessionId: string): void {
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
		withContainer(serverId, (ref) => ref.attachSession(sessionId));
	}

	function killSession(serverId: string, sessionId: string): void {
		containerRefs[serverId]?.killSessionById(sessionId);
	}

	function detachSession(serverId: string, sessionId: string): void {
		const key = serverSessions[serverId]?.find((s) => s.sessionId === sessionId)?.key;
		if (key) containerRefs[serverId]?.detachSession(key);
	}

	function openSession(serverId: string, sessionId: string): void {
		activeServerId = serverId;
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
		containerRefs[serverId]?.openTab(sessionId);
	}

	function newSession(serverId: string, shell?: string): void {
		serverDashboardActive = { ...serverDashboardActive, [serverId]: false };
		withContainer(serverId, (ref) => ref.startSession(shell));
	}

	async function listShells(serverId: string): Promise<{ shells: string[]; defaultShell: string }> {
		return containerRefs[serverId]?.listShells() ?? { shells: [], defaultShell: '' };
	}

	// ── Lifecycle ────────────────────────────────────────────────────

	onMount(() => {
		// ?reset — wipe localStorage and reload clean (dev convenience)
		const params = new URLSearchParams(window.location.search);
		if (params.has('reset')) {
			try { localStorage.removeItem(STORAGE_KEY); } catch {}
			try { localStorage.removeItem(REMOVED_SEEDS_KEY); } catch {}
			// Reload without ?reset so seed servers populate fresh
			window.location.replace(window.location.pathname);
			return;
		}

		const entries = loadServers();
		servers = entries.map(({ connected: _, ...rest }) => rest);

		// Auto-connect servers that were connected last time
		for (const entry of entries) {
			if (entry.connected) {
				connectServer(entry.id);
			}
		}

		// Merge seed servers from dev environment (non-blocking)
		mergeSeedServers();

		// Register keyboard shortcuts
		const cleanups: (() => void)[] = [];

		cleanups.push(keyboard.register({
			key: 't', alt: true,
			description: 'New session on active server',
			action: () => { if (activeServerId) { serverDashboardActive = { ...serverDashboardActive, [activeServerId]: false }; newSession(activeServerId); } }
		}));

		cleanups.push(keyboard.register({
			key: 'w', alt: true,
			description: 'Close active tab',
			action: () => {
				if (!activeServerId) return;
				const key = activeKeys[activeServerId];
				if (key) containerRefs[activeServerId]?.closeTab(key);
			}
		}));

		cleanups.push(keyboard.register({
			key: 'ArrowLeft', alt: true,
			description: 'Previous tab',
			action: () => switchTab(-1)
		}));

		cleanups.push(keyboard.register({
			key: 'ArrowRight', alt: true,
			description: 'Next tab',
			action: () => switchTab(1)
		}));

		// Alt+1-9 for tab switching
		for (let i = 1; i <= 9; i++) {
			cleanups.push(keyboard.register({
				key: String(i), alt: true,
				description: `Switch to tab ${i}`,
				action: () => switchToTabN(i - 1)
			}));
		}

		cleanups.push(keyboard.register({
			key: 'f', alt: true,
			description: 'Toggle terminal search',
			action: () => {
				if (activeServerId) containerRefs[activeServerId]?.toggleSearch();
			}
		}));

		cleanups.push(keyboard.register({
			key: 'k', alt: true,
			description: 'Toggle quick exec bar',
			action: () => { quickExecVisible = !quickExecVisible; }
		}));

		cleanups.push(keyboard.register({
			key: 'p', alt: true,
			description: 'Toggle command palette',
			action: () => { commandPaletteVisible = !commandPaletteVisible; }
		}));

		cleanups.push(keyboard.register({
			key: '/', alt: true,
			description: 'Keyboard shortcuts',
			action: () => {
				const willShow = !shortcutsVisible;
				if (willShow) {
					shortcutsVisible = true;
					// Expand sidebar if collapsed
					if (localStorage.getItem('sctlin-sidebar') === 'false' && sidebarToggleFn) {
						sidebarToggleFn();
						shortcutsAutoExpanded = true;
					}
				} else {
					shortcutsVisible = false;
					// Re-collapse if we auto-expanded
					if (shortcutsAutoExpanded && sidebarToggleFn) {
						sidebarToggleFn();
						shortcutsAutoExpanded = false;
					}
				}
			}
		}));

		cleanups.push(keyboard.register({
			key: 'e', alt: true,
			description: 'Toggle file browser',
			action: () => {
				if (dashboardActive && activeServerId) {
					toggleSidePanel('files', dashPanelKey(activeServerId));
				} else {
					toggleSidePanel('files');
				}
			}
		}));

		cleanups.push(keyboard.register({
			key: 'b', alt: true,
			description: 'Toggle playbook panel',
			action: () => {
				if (dashboardActive && activeServerId) {
					toggleSidePanel('playbooks', dashPanelKey(activeServerId));
				} else {
					toggleSidePanel('playbooks');
				}
			}
		}));

		cleanups.push(keyboard.register({
			key: 'i', alt: true,
			description: 'Server dashboard',
			action: () => {
				if (activeServerId) {
					serverDashboardActive = { ...serverDashboardActive, [activeServerId]: true };
				}
			}
		}));

		cleanups.push(keyboard.register({
			key: '\\', alt: true,
			description: 'Split terminal vertically',
			action: () => {
				if (activeServerId) containerRefs[activeServerId]?.splitVertical();
			}
		}));

		cleanups.push(keyboard.register({
			key: '-', alt: true,
			description: 'Split terminal horizontally',
			action: () => {
				if (activeServerId) containerRefs[activeServerId]?.splitHorizontal();
			}
		}));

		cleanups.push(keyboard.register({
			key: 'q', alt: true,
			description: 'Close split pane',
			action: () => {
				if (activeServerId) containerRefs[activeServerId]?.unsplit();
			}
		}));

		cleanups.push(keyboard.register({
			key: '[', alt: true,
			description: 'Toggle split focus',
			action: () => {
				if (activeServerId) containerRefs[activeServerId]?.toggleSplitFocus();
			}
		}));

		return () => {
			for (const fn of cleanups) fn();
			manager.destroy();
		};
	});

	// ── Tab navigation helpers ──────────────────────────────────────

	function switchTab(direction: number): void {
		if (unifiedSessions.length === 0) return;
		const currentKey = unifiedActiveKey;
		const currentIdx = unifiedSessions.findIndex((s) => s.key === currentKey);
		const newIdx = (currentIdx + direction + unifiedSessions.length) % unifiedSessions.length;
		const target = unifiedSessions[newIdx];
		if (target?.serverId) {
			activeServerId = target.serverId;
			serverDashboardActive = { ...serverDashboardActive, [target.serverId]: false };
			containerRefs[target.serverId]?.selectSession(target.key);
		}
	}

	function switchToTabN(index: number): void {
		if (index >= unifiedSessions.length) return;
		const target = unifiedSessions[index];
		if (target?.serverId) {
			activeServerId = target.serverId;
			serverDashboardActive = { ...serverDashboardActive, [target.serverId]: false };
			containerRefs[target.serverId]?.selectSession(target.key);
		}
	}

	function resetState(): void {
		try { localStorage.removeItem(STORAGE_KEY); } catch {}
		location.reload();
	}

	// ── Quick exec handler ──────────────────────────────────────────

	async function handleQuickExec(command: string) {
		if (!activeServerId) throw new Error('No server connected');
		const conn = manager.get(activeServerId);
		if (!conn) throw new Error('No REST client available');
		return conn.restClient.exec(command);
	}
</script>

<svelte:head>
	<title>sctlin</title>
</svelte:head>

<svelte:window onkeydowncapture={(e) => keyboard.handleKeydown(e)} />

<div class="h-screen bg-neutral-950 flex flex-col">
	<!-- Main area -->
	<div class="flex-1 flex min-h-0">
		<!-- Sidebar -->
		<AppSidebar config={sidebarConfig} class="bg-neutral-950 border-neutral-800">
			{#snippet header(ctx)}
				{#if shortcutsVisible && !ctx.collapsed}
					<div class="animate-[fadeIn_250ms_ease-out_150ms_both]">
						<KeyboardShortcuts shortcuts={keyboard.getAll()} expandedWidth={ctx.expandedWidth} />
					</div>
				{:else}
					<ServerPanel
						{servers}
						{connectionStatuses}
						{serverSessions}
						{serverRemoteSessions}
						{activeServerId}
						activeSessionId={getActiveSessionId()}
						collapsed={ctx.collapsed}
						collapsedWidth={ctx.collapsedWidth}
						{serverSplitGroups}
						{focusedPanes}
						onconnect={connectServer}
						ondisconnect={disconnectServer}
						onselectsession={selectSession}
						onattachsession={attachSession}
						onkillsession={killSession}
						ondetachsession={detachSession}
						onopensession={openSession}
						onnewsession={newSession}
						onlistshells={listShells}
						onaddserver={addServer}
						onremoveserver={removeServer}
						oneditserver={editServer}
					/>
				{/if}
			{/snippet}
			{#snippet toggleBar({ collapsed, toggle })}
				{@const _ = (sidebarToggleFn = toggle)}
				<div class="border-t border-neutral-800 bg-neutral-950">
					<!-- ? button row — slides in above chevron when collapsed, hidden when expanded -->
					<div class="overflow-hidden transition-[max-height] duration-300 ease-in-out"
						style="max-height: {collapsed ? '28px' : '0px'}"
					>
						<div class="flex items-center justify-center h-7">
							<button
								class="w-5 h-5 flex items-center justify-center transition-colors
									{shortcutsVisible ? 'text-green-400' : 'text-neutral-600 hover:text-green-400'}"
								title="Keyboard shortcuts (Alt+/)"
								onclick={() => { const willShow = !shortcutsVisible; toggle(); shortcutsVisible = willShow; shortcutsAutoExpanded = willShow; }}
							>
								<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
									<circle cx="12" cy="12" r="10" />
									<path stroke-linecap="round" d="M9.09 9a3 3 0 015.83 1c0 2-3 3-3 3M12 17h.01" />
								</svg>
							</button>
						</div>
					</div>
					<!-- Main bottom row — chevron always in dot column, ? and reset fade out/in -->
					<div class="flex items-center h-7">
						<div class="shrink-0 flex items-center justify-center" style="width: 36px">
							<button
								class="w-5 h-5 flex items-center justify-center text-neutral-600 hover:text-neutral-400 transition-colors"
								onclick={() => { if (!collapsed) { shortcutsVisible = false; } toggle(); shortcutsAutoExpanded = false; resetConfirming = false; }}
								aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
							>
								<svg
									class="w-3.5 h-3.5 transition-transform duration-300"
									style="transform: rotate({collapsed ? 0 : -180}deg)"
									fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"
								>
									<path stroke-linecap="round" stroke-linejoin="round" d="M13 5l7 7-7 7M5 5l7 7-7 7" />
								</svg>
							</button>
						</div>
						<!-- Inline ? + reset — fade out immediately on collapse, fade in after expand -->
						<div class="flex-1 flex items-center justify-center transition-opacity {collapsed ? 'pointer-events-none' : ''}"
							style="opacity: {collapsed ? 0 : 1}; transition-duration: {collapsed ? '100ms' : '200ms'}; transition-delay: {collapsed ? '0ms' : '200ms'};"
						>
							<button
								class="w-5 h-5 flex items-center justify-center transition-colors
									{shortcutsVisible ? 'text-green-400' : 'text-neutral-600 hover:text-green-400'}"
								title="Keyboard shortcuts (Alt+/)"
								onclick={() => { shortcutsVisible = !shortcutsVisible; shortcutsAutoExpanded = false; }}
							>
								<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
									<circle cx="12" cy="12" r="10" />
									<path stroke-linecap="round" d="M9.09 9a3 3 0 015.83 1c0 2-3 3-3 3M12 17h.01" />
								</svg>
							</button>
						</div>
						<div class="transition-opacity {collapsed ? 'pointer-events-none' : ''}"
							style="opacity: {collapsed ? 0 : 1}; transition-duration: {collapsed ? '100ms' : '200ms'}; transition-delay: {collapsed ? '0ms' : '200ms'};"
						>
							<button
								class="mr-2 w-5 h-5 flex items-center justify-center transition-colors
									{resetConfirming ? 'text-red-400' : 'text-neutral-600 hover:text-yellow-500'}"
								title={resetConfirming ? 'Click again to reset' : 'Reset to defaults'}
								onclick={() => {
									if (resetConfirming) { resetState(); resetConfirming = false; }
									else { resetConfirming = true; }
								}}
								onmouseleave={() => { resetConfirming = false; }}
							>
								<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
									<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
								</svg>
							</button>
						</div>
					</div>
				</div>
			{/snippet}
		</AppSidebar>

		<!-- Terminal area: unified tabs + stacked containers -->
		<div class="flex-1 min-w-0 flex flex-col relative">
			<!-- Tab bar: [server1] [s1-tabs...] [server2] [s2-tabs...] [+] -->
			{#if connectedServers.length > 0}
				<!-- svelte-ignore a11y_no_static_element_interactions -->
				<div class="flex items-center h-8 shrink-0 bg-neutral-900 overflow-x-auto scrollbar-none"
					onwheel={(e) => { if (e.deltaY) { e.preventDefault(); e.currentTarget.scrollLeft += e.deltaY; } }}>
					{#each connectedServers as server, serverIdx (server.id)}
						{@const isActive = server.id === activeServerId}
						{@const isDash = isActive && (serverDashboardActive[server.id] ?? false)}
						<!-- Server dashboard tab -->
						<div
							role="tab"
							tabindex="0"
							aria-selected={isDash}
							class="group flex items-center px-2 h-full text-[10px] leading-none transition-colors whitespace-nowrap cursor-pointer select-none shrink-0
								{serverIdx > 0 ? 'border-l border-l-neutral-800' : ''}
								{isDash
									? 'bg-neutral-800 text-green-400/80'
									: isActive
										? 'text-green-600/60 hover:bg-neutral-800/50 hover:text-green-400/70'
										: 'text-green-600/60 hover:bg-neutral-800/50 hover:text-green-500/60'}"
							onclick={() => {
								activeServerId = server.id;
								serverDashboardActive = { ...serverDashboardActive, [server.id]: true };
								activeViewerKey = { ...activeViewerKey, [server.id]: null };
							}}
							onkeydown={(e) => {
								if (e.key === 'Enter' || e.key === ' ') {
									e.preventDefault();
									activeServerId = server.id;
									serverDashboardActive = { ...serverDashboardActive, [server.id]: true };
									activeViewerKey = { ...activeViewerKey, [server.id]: null };
								}
							}}
						>
							<span class="font-mono translate-y-px truncate max-w-28">{server.name}</span>
						</div>
						<!-- Session tabs for this server -->
						<TerminalTabs
							inline
							sessions={serverSessions[server.id] ?? []}
							activeSessionId={isActive && !isDash && !activeViewerKey[server.id] ? (activeKeys[server.id] ?? null) : null}
							splitGroups={serverSplitGroups[server.id] ?? []}
							focusedPane={isActive ? (focusedPanes[server.id] ?? 'primary') : 'primary'}
							onselect={(key) => {
								activeServerId = server.id;
								serverDashboardActive = { ...serverDashboardActive, [server.id]: false };
								activeViewerKey = { ...activeViewerKey, [server.id]: null };
								containerRefs[server.id]?.selectSession(key);
							}}
							onclose={(key) => {
								containerRefs[server.id]?.closeTab(key);
							}}
							onrename={(key, label) => {
								containerRefs[server.id]?.renameSession(key, label);
							}}
							ondotclick={(key) => {
								const s = (serverSessions[server.id] ?? []).find((x) => x.key === key);
								if (!s) return;
								if (s.attached) containerRefs[server.id]?.detachSession(key);
								else containerRefs[server.id]?.attachSession(s.sessionId);
							}}
							onunsplit={(primaryKey) => {
								const ref = containerRefs[server.id];
								if (ref) { ref.selectSession(primaryKey); ref.unsplit(); }
							}}
						/>
						<!-- Viewer tabs for this server -->
						{#each serverViewerTabs[server.id] ?? [] as viewer (viewer.key)}
							{@const isVActive = isActive && activeViewerKey[server.id] === viewer.key}
							<div
								role="tab"
								tabindex="0"
								aria-selected={isVActive}
								class="group flex items-center gap-0.5 pl-1 pr-1 h-full text-[10px] leading-none transition-colors whitespace-nowrap cursor-pointer select-none
									{isVActive
										? 'bg-neutral-800 text-neutral-200'
										: 'text-neutral-500 hover:bg-neutral-800/50 hover:text-neutral-300'}"
								onclick={() => selectViewerTab(server.id, viewer.key)}
								onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); selectViewerTab(server.id, viewer.key); } }}
							>
								<span class="w-4 h-4 mr-0.5 flex items-center justify-center text-amber-400/70 text-[9px]">{viewer.icon}</span>
								<span class="font-mono translate-y-px truncate max-w-24">{viewer.label}</span>
								<div class="overflow-hidden transition-all duration-150" style="width: {isVActive ? '16px' : '0px'}">
									<button
										class="w-4 h-4 flex items-center justify-center rounded text-neutral-400 hover:bg-neutral-600/50 hover:text-red-400"
										onclick={(e) => { e.stopPropagation(); closeViewerTab(server.id, viewer.key); }}
										aria-label="Close tab"
									>
										<svg class="w-2.5 h-2.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
											<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
										</svg>
									</button>
								</div>
							</div>
						{/each}
					{/each}
					<div class="flex-1 min-w-0"></div>
					<!-- New session button -->
					{#if activeServerId}
						<button
							class="shrink-0 w-8 h-8 flex items-center justify-center text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800/50 transition-colors"
							onclick={() => { if (activeServerId) { serverDashboardActive = { ...serverDashboardActive, [activeServerId]: false }; newSession(activeServerId); } }}
							title="New session (Alt+T)"
						>
							<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
								<path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" />
							</svg>
						</button>
					{/if}
					<!-- Transfer indicator -->
					<TransferIndicator
						transfers={activeTransferList}
						onabort={(id: string) => activeTransferTracker?.abort(id)}
						ondismiss={(id: string) => activeTransferTracker?.dismiss(id)}
					/>
				</div>
			{/if}

			<!-- Container stack (terminal area) -->
			<div class="flex-1 relative min-h-0">
				<!-- Logo in background -->
				<div class="absolute inset-0 flex items-center justify-center bg-neutral-950 pointer-events-none">
					<img src="/sctl-logo.png" alt="sctl" class="max-w-full max-h-full w-auto h-auto opacity-90" style="object-fit: contain;" />
				</div>

				<!-- Terminal containers -->
				{#each servers as server (server.id)}
					{#if serverConfigs[server.id]}
						<div
							class="absolute inset-0"
							style:visibility={server.id === activeServerId && !dashboardActive && !activeViewerKey[server.id] ? 'visible' : 'hidden'}
						>
							<TerminalContainer
								config={serverConfigs[server.id]}
								showTabs={false}
								onToggleFiles={() => { toggleSidePanel('files', focusedKeyFor(server.id) ?? undefined); }}
								onTogglePlaybooks={() => { toggleSidePanel('playbooks', focusedKeyFor(server.id) ?? undefined); }}
								sidePanelOpen={serverPanelOpen[server.id] ?? false}
								sidePanelTab={sessionPanelTab[focusedKeyFor(server.id) ?? ''] ?? 'files'}
								rightInset={(serverPanelOpen[server.id] ?? false) ? panelWidth : 0}
								rightInsetAnimate={panelAnimating && !panelResizing}
								bind:this={containerRefs[server.id]}
							/>
						</div>
					{/if}
				{/each}

				<!-- Server dashboard layer -->
				{#each servers as server (server.id)}
					{#if serverConfigs[server.id]}
						{@const dpk = dashPanelKey(server.id)}
						{@const isDashVisible = server.id === activeServerId && serverDashboardActive[server.id]}
						<div
							class="absolute inset-0 z-10 bg-neutral-950"
							style:visibility={isDashVisible ? 'visible' : 'hidden'}
						>
							<ServerDashboard
								visible={!!isDashVisible}
								connectionStatus={connectionStatuses[server.id] ?? 'disconnected'}
								deviceInfo={serverDeviceInfo[server.id] ?? null}
								activity={serverActivity[server.id] ?? []}
								restClient={manager.get(server.id)?.restClient ?? null}
								onrefreshinfo={() => manager.fetchDeviceInfo(server.id)}
								onToggleFiles={() => { toggleSidePanel('files', dpk); }}
								onTogglePlaybooks={() => { toggleSidePanel('playbooks', dpk); }}
								onToggleAi={() => { toggleMasterAi(server.id); }}
								onOpenViewer={(tab) => openViewerTab(server.id, tab)}
								sidePanelOpen={serverPanelOpen[server.id] ?? false}
								sidePanelTab={sessionPanelTab[dpk] ?? 'files'}
								masterAiEnabled={serverMasterAi[server.id] ?? false}
								rightInset={(serverPanelOpen[server.id] ?? false) ? panelWidth : 0}
								rightInsetAnimate={panelAnimating && !panelResizing}
							/>
						</div>
					{/if}
				{/each}

				<!-- Viewer tab layer -->
				{#each servers as server (server.id)}
					{#if serverConfigs[server.id]}
						{#each serverViewerTabs[server.id] ?? [] as viewer (viewer.key)}
							{@const isViewerVisible = server.id === activeServerId && activeViewerKey[server.id] === viewer.key}
							<div
								class="absolute inset-0 z-10 bg-neutral-950"
								style:visibility={isViewerVisible ? 'visible' : 'hidden'}
							>
								{#if viewer.type === 'exec'}
									<ExecViewer data={viewer.data as ExecViewerData} onclose={() => closeViewerTab(server.id, viewer.key)} />
								{/if}
							</div>
						{/each}
					{/if}
				{/each}
			</div>

			<!-- Side panel (per-session, overlays from top-right, stops above ControlBar) -->
			{#each servers as server (server.id)}
				{#if serverConfigs[server.id]}
					{#each serverSessions[server.id] ?? [] as session (session.key)}
						{@const isFocused = server.id === activeServerId && !activeViewerKey[server.id] && session.key === focusedKeyFor(server.id)}
						{@const panelTab = sessionPanelTab[session.key] ?? 'files'}
						<div
							class="absolute top-0 right-0 z-10"
							style="bottom: 28px;"
							style:visibility={isFocused ? 'visible' : 'hidden'}
						>
							<SidePanel
								open={isFocused && (serverPanelOpen[server.id] ?? false)}
								width={panelWidth}
								animate={panelAnimating}
								onwidthchange={(w) => { panelResizing = true; panelWidth = w; }}
								onresizeend={() => { panelResizing = false; }}
							>
								{#snippet children()}
									<div class="h-full" style:display={panelTab === 'files' ? 'flex' : 'none'}>
										<FileBrowser
											visible={isFocused && (serverPanelOpen[server.id] ?? false) && panelTab === 'files'}
											restClient={manager.get(server.id)?.restClient ?? null}
											tracker={manager.get(server.id)?.transferTracker ?? null}
											onsynccd={(path) => {
												containerRefs[server.id]?.execInActiveSession(`cd ${path}`);
											}}
										/>
									</div>
									<div class="h-full" style:display={panelTab === 'playbooks' ? 'flex' : 'none'}>
										<PlaybookPanel
											visible={isFocused && (serverPanelOpen[server.id] ?? false) && panelTab === 'playbooks'}
											restClient={manager.get(server.id)?.restClient ?? null}
											onRunInTerminal={(script: string) => {
												containerRefs[server.id]?.execInActiveSession(script);
											}}
										/>
									</div>
								{/snippet}
							</SidePanel>
						</div>
					{/each}
				{/if}
			{/each}

			<!-- Side panel for dashboard view -->
			{#each servers as server (server.id)}
				{#if serverConfigs[server.id]}
					{@const dpk = dashPanelKey(server.id)}
					{@const dashTab = sessionPanelTab[dpk] ?? 'files'}
					{@const isDashFocused = server.id === activeServerId && dashboardActive}
					<div
						class="absolute top-0 right-0 z-10"
						style="bottom: 28px;"
						style:visibility={isDashFocused ? 'visible' : 'hidden'}
					>
						<SidePanel
							open={isDashFocused && (serverPanelOpen[server.id] ?? false)}
							width={panelWidth}
							animate={panelAnimating}
							onwidthchange={(w) => { panelResizing = true; panelWidth = w; }}
								onresizeend={() => { panelResizing = false; }}
						>
							{#snippet children()}
								<div class="h-full" style:display={dashTab === 'files' ? 'flex' : 'none'}>
									<FileBrowser
										visible={isDashFocused && (serverPanelOpen[server.id] ?? false) && dashTab === 'files'}
										restClient={manager.get(server.id)?.restClient ?? null}
										tracker={manager.get(server.id)?.transferTracker ?? null}
									/>
								</div>
								<div class="h-full" style:display={dashTab === 'playbooks' ? 'flex' : 'none'}>
									<PlaybookPanel
										visible={isDashFocused && (serverPanelOpen[server.id] ?? false) && dashTab === 'playbooks'}
										restClient={manager.get(server.id)?.restClient ?? null}
									/>
								</div>
							{/snippet}
						</SidePanel>
					</div>
				{/if}
			{/each}
		</div>
	</div>
</div>

<!-- Toast notifications -->
<ToastContainer bind:this={toastRef} />

<!-- Quick exec overlay -->
<QuickExecBar
	visible={quickExecVisible}
	serverName={activeServer?.name}
	onexec={handleQuickExec}
	onclose={() => { quickExecVisible = false; }}
/>

<!-- Command palette overlay -->
<CommandPalette
	visible={commandPaletteVisible}
	shortcuts={keyboard.getAll()}
	onclose={() => { commandPaletteVisible = false; }}
/>

