<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { TerminalContainer, TerminalTabs, ServerPanel, ToastContainer, QuickExecBar, FileBrowser, CommandPalette } from '$lib';
	import { SctlRestClient } from '$lib/utils/rest-client';
	import { KeyboardManager } from '$lib/utils/keyboard';
	import { AppSidebar } from 'gawdux';
	import type { SidebarConfig } from 'gawdux';
	import type {
		SctlinConfig,
		SessionInfo,
		ConnectionStatus,
		RemoteSessionInfo,
		ServerConfig,
		DeviceInfo,
		ActivityEntry
	} from '$lib';

	// ── Persistence ──────────────────────────────────────────────────

	const STORAGE_KEY = 'sctlin-servers';
	const OLD_STORAGE_KEY = 'sctlin-dev-settings';

	interface ServerEntry extends ServerConfig {
		connected: boolean;
	}

	const DEFAULT_SERVER: ServerEntry = {
		id: 'default',
		name: 'localhost',
		wsUrl: 'ws://localhost:1337/api/ws',
		apiKey: 'dev-key',
		shell: '',
		connected: false
	};

	function loadServers(): ServerEntry[] {
		try {
			const raw = localStorage.getItem(STORAGE_KEY);
			if (raw) {
				const parsed = JSON.parse(raw);
				if (Array.isArray(parsed) && parsed.length > 0) return parsed;
			}
		} catch {}

		// Migrate from old format
		try {
			const old = localStorage.getItem(OLD_STORAGE_KEY);
			if (old) {
				const parsed = JSON.parse(old);
				const migrated: ServerEntry = {
					id: 'default',
					name: 'localhost',
					wsUrl: parsed.wsUrl || DEFAULT_SERVER.wsUrl,
					apiKey: parsed.apiKey || DEFAULT_SERVER.apiKey,
					shell: parsed.shell || '',
					connected: false
				};
				// Check if was previously connected
				try {
					if (sessionStorage.getItem('sctlin-connected') === 'true') {
						migrated.connected = true;
					}
				} catch {}
				localStorage.removeItem(OLD_STORAGE_KEY);
				try { sessionStorage.removeItem('sctlin-connected'); } catch {}
				return [migrated];
			}
		} catch {}

		return [DEFAULT_SERVER];
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

	// REST clients + device info
	let serverRestClients: Record<string, SctlRestClient> = {};
	let serverDeviceInfo: Record<string, DeviceInfo | null> = $state({});

	// Activity feed
	let serverActivity: Record<string, ActivityEntry[]> = $state({});

	// Toast
	let toastRef: ToastContainer | undefined = $state();

	// Keyboard
	const keyboard = new KeyboardManager();

	// Quick exec
	let quickExecVisible = $state(false);

	// File browser (per-session open state, shared width)
	let fileBrowserOpen: Record<string, boolean> = $state({});
	const FB_WIDTH_KEY = 'sctlin-filebrowser-width';
	let fileBrowserWidth = $state(
		(() => { try { return parseInt(localStorage.getItem(FB_WIDTH_KEY) ?? '') || 384; } catch { return 384; } })()
	);
	let animatingSessionKey: string | null = $state(null);

	// Persist shared width
	$effect(() => { try { localStorage.setItem(FB_WIDTH_KEY, String(fileBrowserWidth)); } catch {} });

	// Command palette
	let commandPaletteVisible = $state(false);

	const sidebarConfig: SidebarConfig = {
		storageKey: 'sctlin-sidebar',
		defaultCollapsed: true,
		expandedWidth: 256,
		collapsedWidth: 48,
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

	let activeFileBrowserOpen = $derived(
		activeSessionKey ? !!fileBrowserOpen[activeSessionKey] : false
	);

	function toggleFileBrowser(sessionKey?: string) {
		const key = sessionKey ?? activeSessionKey;
		if (!key) return;
		animatingSessionKey = key;
		fileBrowserOpen = { ...fileBrowserOpen, [key]: !fileBrowserOpen[key] };
		setTimeout(() => { animatingSessionKey = null; }, 350);
	}

	function getActiveSessionId(): string | null {
		if (!activeServerId) return null;
		const key = activeKeys[activeServerId];
		if (!key) return null;
		return serverSessions[activeServerId]?.find((s) => s.key === key)?.sessionId ?? null;
	}

	// Unified flat session list across all servers (for the unified tab bar)
	let unifiedSessions = $derived(
		Object.entries(serverSessions).flatMap(([serverId, list]) => {
			const server = servers.find((s) => s.id === serverId);
			return list.map((s) => ({
				...s,
				serverId,
				serverName: servers.length > 1 ? (server?.name ?? serverId) : undefined
			}));
		})
	);

	// Which session key is active in the unified bar
	let unifiedActiveKey = $derived(
		activeServerId ? activeKeys[activeServerId] ?? null : null
	);

	// ── Config builder ───────────────────────────────────────────────

	function buildConfig(server: ServerConfig): SctlinConfig {
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
				workingDir: '~'
			},
			callbacks: {
				onConnectionChange: (status) => {
					const prevStatus = connectionStatuses[server.id];
					connectionStatuses = { ...connectionStatuses, [server.id]: status };

					// Toast on connection status transitions
					if (status === 'connected' && prevStatus !== 'connected') {
						toastRef?.push(`Connected to ${server.name}`, 'success');
						// Fetch device info + activity
						fetchDeviceInfo(server.id);
						fetchActivity(server.id);
					} else if (status === 'reconnecting' && prevStatus === 'connected') {
						toastRef?.push(`Reconnecting to ${server.name}...`, 'warning');
					} else if (status === 'disconnected' && prevStatus === 'connected') {
						toastRef?.push(`Disconnected from ${server.name}`, 'info');
					}
				},
				onRemoteSessions: (sessions) => {
					serverRemoteSessions = { ...serverRemoteSessions, [server.id]: sessions };
				},
				onSessionsChange: (sessions) => {
					serverSessions = { ...serverSessions, [server.id]: sessions };
				},
				onActiveSessionChange: (key) => {
					activeKeys = { ...activeKeys, [server.id]: key };
				},
				onActivity: (entry) => {
					const current = serverActivity[server.id] ?? [];
					// Deduplicate: REST fetch on connect may overlap with WS broadcast
					if (current.some((e) => e.id === entry.id)) return;
					const updated = [...current, entry];
					// Cap at 200 entries
					serverActivity = {
						...serverActivity,
						[server.id]: updated.length > 200 ? updated.slice(-200) : updated
					};
				},
				onError: (err) => {
					console.error(`[sctlin:${server.name}]`, err.message);
					toastRef?.push(err.message, 'error');
				}
			}
		};
	}

	// ── Device info ─────────────────────────────────────────────────

	async function fetchDeviceInfo(serverId: string): Promise<void> {
		const client = serverRestClients[serverId];
		if (!client) return;
		try {
			const info = await client.getInfo();
			serverDeviceInfo = { ...serverDeviceInfo, [serverId]: info };
		} catch (err) {
			console.error(`Failed to fetch device info for ${serverId}:`, err);
			serverDeviceInfo = { ...serverDeviceInfo, [serverId]: null };
		}
	}

	// ── Activity feed ───────────────────────────────────────────────

	async function fetchActivity(serverId: string): Promise<void> {
		const client = serverRestClients[serverId];
		if (!client) return;
		try {
			const entries = await client.getActivity(0, 100);
			serverActivity = { ...serverActivity, [serverId]: entries };
		} catch (err) {
			console.error(`Failed to fetch activity for ${serverId}:`, err);
		}
	}

	// ── Server management ────────────────────────────────────────────

	function connectServer(id: string): void {
		const server = servers.find((s) => s.id === id);
		if (!server || serverConfigs[id]) return;
		serverConfigs = { ...serverConfigs, [id]: buildConfig(server) };
		connectionStatuses = { ...connectionStatuses, [id]: 'connecting' };
		// Create REST client
		serverRestClients[id] = new SctlRestClient(server.wsUrl, server.apiKey);
		if (!activeServerId) activeServerId = id;
		saveServers();
	}

	function disconnectServer(id: string): void {
		const { [id]: _, ...rest } = serverConfigs;
		serverConfigs = rest;
		connectionStatuses = { ...connectionStatuses, [id]: 'disconnected' };
		serverSessions = { ...serverSessions, [id]: [] };
		serverRemoteSessions = { ...serverRemoteSessions, [id]: [] };
		delete containerRefs[id];
		delete serverRestClients[id];
		const { [id]: _di, ...restDi } = serverDeviceInfo;
		serverDeviceInfo = restDi;
		const { [id]: _act, ...restAct } = serverActivity;
		serverActivity = restAct;
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

	function selectSession(serverId: string, sessionId: string): void {
		activeServerId = serverId;
		const key = serverSessions[serverId]?.find((s) => s.sessionId === sessionId)?.key;
		if (key) containerRefs[serverId]?.selectSession(key);
	}

	function attachSession(serverId: string, sessionId: string): void {
		activeServerId = serverId;
		containerRefs[serverId]?.attachSession(sessionId);
	}

	function killSession(serverId: string, sessionId: string): void {
		const key = serverSessions[serverId]?.find((s) => s.sessionId === sessionId)?.key;
		if (key) containerRefs[serverId]?.closeSession(key);
	}

	function detachSession(serverId: string, sessionId: string): void {
		const key = serverSessions[serverId]?.find((s) => s.sessionId === sessionId)?.key;
		if (key) containerRefs[serverId]?.detachSession(key);
	}

	function openSession(serverId: string, sessionId: string): void {
		activeServerId = serverId;
		containerRefs[serverId]?.openTab(sessionId);
	}

	function newSession(serverId: string, shell?: string): void {
		activeServerId = serverId;
		if (!serverConfigs[serverId]) {
			connectServer(serverId);
		}
		// Wait for WS to be connected before starting session
		if (connectionStatuses[serverId] === 'connected') {
			containerRefs[serverId]?.startSession(shell);
		} else {
			const unsub = $effect.root(() => {
				$effect(() => {
					if (connectionStatuses[serverId] === 'connected') {
						unsub();
						containerRefs[serverId]?.startSession(shell);
					}
				});
			});
			// Safety timeout: clean up if connection never succeeds
			setTimeout(() => { try { unsub(); } catch {} }, 15000);
		}
	}

	async function listShells(serverId: string): Promise<{ shells: string[]; defaultShell: string }> {
		return containerRefs[serverId]?.listShells() ?? { shells: [], defaultShell: '' };
	}

	// ── Lifecycle ────────────────────────────────────────────────────

	onMount(() => {
		const entries = loadServers();
		servers = entries.map(({ connected: _, ...rest }) => rest);

		// Auto-connect servers that were connected last time
		for (const entry of entries) {
			if (entry.connected) {
				connectServer(entry.id);
			}
		}

		// Register keyboard shortcuts
		const cleanups: (() => void)[] = [];

		cleanups.push(keyboard.register({
			key: 't', alt: true,
			description: 'New session on active server',
			action: () => { if (activeServerId) newSession(activeServerId); }
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

		return () => {
			for (const fn of cleanups) fn();
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
			containerRefs[target.serverId]?.selectSession(target.key);
		}
	}

	function switchToTabN(index: number): void {
		if (index >= unifiedSessions.length) return;
		const target = unifiedSessions[index];
		if (target?.serverId) {
			activeServerId = target.serverId;
			containerRefs[target.serverId]?.selectSession(target.key);
		}
	}

	function resetState(): void {
		try { localStorage.removeItem(STORAGE_KEY); } catch {}
		location.reload();
	}

	// ── Status dot helper (for header) ──────────────────────────────

	function headerDotColor(status: ConnectionStatus | null): string {
		switch (status) {
			case 'connected':
				return 'bg-green-500';
			case 'connecting':
			case 'reconnecting':
				return 'bg-yellow-500 animate-pulse';
			default:
				return 'bg-neutral-600';
		}
	}

	// ── Quick exec handler ──────────────────────────────────────────

	async function handleQuickExec(command: string) {
		if (!activeServerId) throw new Error('No server connected');
		const client = serverRestClients[activeServerId];
		if (!client) throw new Error('No REST client available');
		return client.exec(command);
	}
</script>

<svelte:head>
	<title>sctlin</title>
</svelte:head>

<svelte:window onkeydown={(e) => keyboard.handleKeydown(e)} />

<div class="h-screen bg-neutral-950 flex flex-col">
	<!-- Main area -->
	<div class="flex-1 flex min-h-0">
		<!-- Sidebar -->
		<AppSidebar config={sidebarConfig} class="bg-neutral-950 border-neutral-800">
			{#snippet header(ctx)}
				<ServerPanel
					{servers}
					{connectionStatuses}
					{serverSessions}
					{serverRemoteSessions}
					{activeServerId}
					activeSessionId={getActiveSessionId()}
					collapsed={ctx.collapsed}
					collapsedWidth={ctx.collapsedWidth}
					{serverDeviceInfo}
					{serverActivity}
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
					onrefreshinfo={fetchDeviceInfo}
				/>
			{/snippet}
			{#snippet toggleBar({ collapsed, toggle })}
				<div class="flex items-center border-t border-neutral-800 bg-neutral-950 py-1 text-[10px] h-7">
					<button
						class="w-12 flex items-center justify-center text-neutral-600 hover:text-neutral-400 transition-colors"
						onclick={toggle}
						aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
					>
						<svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							{#if collapsed}
								<path stroke-linecap="round" stroke-linejoin="round" d="M13 5l7 7-7 7M5 5l7 7-7 7" />
							{:else}
								<path stroke-linecap="round" stroke-linejoin="round" d="M11 19l-7-7 7-7M19 19l-7-7 7-7" />
							{/if}
						</svg>
					</button>
					{#if !collapsed}
						<div class="flex-1"></div>
						<button
							class="mr-2 text-neutral-600 hover:text-red-400 transition-colors"
							title="Reset to defaults"
							onclick={resetState}
						>
							<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
								<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
							</svg>
						</button>
					{/if}
				</div>
			{/snippet}
		</AppSidebar>

		<!-- Terminal area: unified tabs + stacked containers -->
		<div class="flex-1 min-w-0 flex flex-col relative">
			<!-- Unified tab bar across all servers -->
			{#if unifiedSessions.length > 0}
				<div class="flex items-center border-b border-neutral-700 h-8 shrink-0">
					<div class="flex-1 min-w-0">
						<TerminalTabs
							sessions={unifiedSessions}
							activeSessionId={unifiedActiveKey}
							onselect={(key) => {
								const s = unifiedSessions.find((x) => x.key === key);
								if (s?.serverId) {
									activeServerId = s.serverId;
									containerRefs[s.serverId]?.selectSession(key);
								}
							}}
							onclose={(key) => {
								const s = unifiedSessions.find((x) => x.key === key);
								if (s?.serverId) containerRefs[s.serverId]?.closeTab(key);
							}}
							onrename={(key, label) => {
								const s = unifiedSessions.find((x) => x.key === key);
								if (s?.serverId) containerRefs[s.serverId]?.renameSession(key, label);
							}}
							ondotclick={(key) => {
								const s = unifiedSessions.find((x) => x.key === key);
								if (!s?.serverId) return;
								if (s.attached) containerRefs[s.serverId]?.detachSession(key);
								else containerRefs[s.serverId]?.attachSession(s.sessionId);
							}}
						/>
					</div>
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
							style:visibility={server.id === activeServerId ? 'visible' : 'hidden'}
						>
							<TerminalContainer
								config={serverConfigs[server.id]}
								showTabs={false}
								onToggleFileBrowser={() => { toggleFileBrowser(activeKeys[server.id] ?? undefined); }}
								fileBrowserOpen={!!fileBrowserOpen[activeKeys[server.id] ?? '']}
								rightInset={fileBrowserOpen[activeKeys[server.id] ?? ''] ? fileBrowserWidth : 0}
								rightInsetAnimate={animatingSessionKey === activeKeys[server.id]}
								bind:this={containerRefs[server.id]}
							/>
						</div>
					{/if}
				{/each}
			</div>

			<!-- File browser (per-session, overlays from top-right, stops above ControlBar) -->
			{#each servers as server (server.id)}
				{#if serverConfigs[server.id]}
					{#each serverSessions[server.id] ?? [] as session (session.key)}
						<div
							class="absolute top-0 right-0 z-10"
							style="bottom: 28px;"
							style:visibility={server.id === activeServerId && session.key === activeKeys[server.id] ? 'visible' : 'hidden'}
						>
							<FileBrowser
								visible={server.id === activeServerId && session.key === activeKeys[server.id] && !!fileBrowserOpen[session.key]}
								width={fileBrowserWidth}
								animate={animatingSessionKey === session.key}
								restClient={serverRestClients[server.id] ?? null}
								onclose={() => { toggleFileBrowser(session.key); }}
								onwidthchange={(w) => { fileBrowserWidth = w; }}
								onsynccd={(path) => {
									containerRefs[server.id]?.execInActiveSession(`cd ${path}`);
								}}
							/>
						</div>
					{/each}
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
