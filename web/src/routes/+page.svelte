<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { TerminalContainer, TerminalTabs, ServerPanel } from '$lib';
	import { AppSidebar } from 'gawdux';
	import type { SidebarConfig } from 'gawdux';
	import type {
		SctlinConfig,
		SessionInfo,
		ConnectionStatus,
		RemoteSessionInfo,
		ServerConfig
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
					connectionStatuses = { ...connectionStatuses, [server.id]: status };
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
				onError: (err) => console.error(`[sctlin:${server.name}]`, err.message)
			}
		};
	}

	// ── Server management ────────────────────────────────────────────

	function connectServer(id: string): void {
		const server = servers.find((s) => s.id === id);
		if (!server || serverConfigs[id]) return;
		serverConfigs = { ...serverConfigs, [id]: buildConfig(server) };
		connectionStatuses = { ...connectionStatuses, [id]: 'connecting' };
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
	});

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
</script>

<svelte:head>
	<title>sctlin</title>
</svelte:head>

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
		<div class="flex-1 min-w-0 flex flex-col">
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

			<!-- Container stack -->
			<div class="flex-1 relative min-h-0">
				<!-- Logo in background -->
				<div class="absolute inset-0 flex items-center justify-center bg-neutral-950 pointer-events-none">
					<img src="/sctl-logo.png" alt="sctl" class="max-w-full max-h-full w-auto h-auto opacity-90" style="object-fit: contain;" />
				</div>

				{#each servers as server (server.id)}
					{#if serverConfigs[server.id]}
						<div
							class="absolute inset-0"
							style:visibility={server.id === activeServerId ? 'visible' : 'hidden'}
						>
							<TerminalContainer
								config={serverConfigs[server.id]}
								showTabs={false}
								bind:this={containerRefs[server.id]}
							/>
						</div>
					{/if}
				{/each}
			</div>
		</div>
	</div>
</div>
