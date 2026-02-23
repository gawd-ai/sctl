<script lang="ts">
	import { untrack } from 'svelte';
	import type {
		ServerConfig,
		SessionInfo,
		RemoteSessionInfo,
		ConnectionStatus,
		DeviceInfo,
		ActivityEntry
	} from '../types/terminal.types';
	import DeviceInfoPanel from './DeviceInfoPanel.svelte';
	import ActivityFeed from './ActivityFeed.svelte';

	interface Props {
		servers: ServerConfig[];
		connectionStatuses: Record<string, ConnectionStatus>;
		serverSessions: Record<string, SessionInfo[]>;
		serverRemoteSessions: Record<string, RemoteSessionInfo[]>;
		activeServerId: string | null;
		activeSessionId: string | null;
		collapsed: boolean;
		collapsedWidth: number;
		onconnect?: (serverId: string) => void;
		ondisconnect?: (serverId: string) => void;
		onselectsession?: (serverId: string, sessionId: string) => void;
		onattachsession?: (serverId: string, sessionId: string) => void;
		onkillsession?: (serverId: string, sessionId: string) => void;
		ondetachsession?: (serverId: string, sessionId: string) => void;
		onnewsession?: (serverId: string, shell?: string) => void;
		onlistshells?: (serverId: string) => Promise<{ shells: string[]; defaultShell: string }>;
		onopensession?: (serverId: string, sessionId: string) => void;
		onaddserver?: (config: Omit<ServerConfig, 'id'>) => void;
		onremoveserver?: (serverId: string) => void;
		oneditserver?: (serverId: string, updates: Partial<ServerConfig>) => void;
		serverDeviceInfo?: Record<string, DeviceInfo | null>;
		onrefreshinfo?: (serverId: string) => void;
		serverActivity?: Record<string, ActivityEntry[]>;
	}

	let {
		servers,
		connectionStatuses,
		serverSessions,
		serverRemoteSessions,
		activeServerId,
		activeSessionId,
		collapsed,
		collapsedWidth,
		onconnect,
		ondisconnect,
		onselectsession,
		onattachsession,
		onkillsession,
		ondetachsession,
		onnewsession,
		onlistshells,
		onopensession,
		onaddserver,
		onremoveserver,
		oneditserver,
		serverDeviceInfo = {},
		onrefreshinfo,
		serverActivity = {}
	}: Props = $props();

	// ── Internal types ──────────────────────────────────────────────

	interface ServerDisplay {
		id: string;
		name: string;
		host: string;
		status: ConnectionStatus;
		sessionCount: number;
		isActive: boolean;
	}

	interface SessionDisplay {
		id: string;
		serverId: string;
		label: string;
		pid?: number;
		status: 'attached' | 'open' | 'detached';
		isActive: boolean;
		sessionStatus?: 'running' | 'exited';
		idle?: boolean;
		exitCode?: number;
		idleTimeout?: number;
	}

	// ── State ────────────────────────────────────────────────────────

	let expandedServers: Record<string, boolean> = $state({});
	let infoExpanded: Record<string, boolean> = $state({});
	let activityExpanded: Record<string, boolean> = $state({});
	let flyoutServerId: string | null = $state(null);
	let flyoutTop = $state(0);
	let hoverTimeout: ReturnType<typeof setTimeout> | null = null;
	let shellPickerDismissTimer: ReturnType<typeof setTimeout> | null = null;

	// Add server form
	let showAddForm = $state(false);
	let addName = $state('');
	let addWsUrl = $state('ws://localhost:1337/api/ws');
	let addApiKey = $state('');
	let addShell = $state('');

	// Edit server
	let editingServerId: string | null = $state(null);
	let editName = $state('');
	let editWsUrl = $state('');
	let editApiKey = $state('');
	let editShell = $state('');

	// Shell picker
	let shellPickerServerId: string | null = $state(null);
	let shellPickerShells: string[] = $state([]);
	let shellPickerDefault: string = $state('');
	let shellPickerLoading = $state(false);

	// Shell cache — pre-fetched at connect time so picker is instant
	const shellCache = new Map<string, { shells: string[]; defaultShell: string }>();
	const shellFetching = new Set<string>();

	// Confirmation states (reset on mouse leave)
	let confirmingRemoveId: string | null = $state(null);
	let confirmingKillId: string | null = $state(null);

	// Auto-expand tracking (#1) — plain Map to avoid reactive churn
	const prevStatuses = new Map<string, ConnectionStatus>();

	// ── Auto-expand on connect + auto-fetch shells ──────────────────

	$effect(() => {
		for (const [id, status] of Object.entries(connectionStatuses)) {
			const prev = prevStatuses.get(id);
			if (prev !== 'connected' && status === 'connected') {
				expandedServers = { ...expandedServers, [id]: true };
				// Pre-fetch shells so the picker is instant
				if (!shellCache.has(id) && !shellFetching.has(id)) {
					untrack(() => fetchShellsForServer(id));
				}
			}
			if (prev === 'connected' && status === 'disconnected') {
				shellCache.delete(id);
				shellFetching.delete(id);
				untrack(() => {
					if (shellPickerServerId === id) closeShellPicker();
				});
			}
			prevStatuses.set(id, status);
		}
	});

	// ── Dismiss flyout on collapse/expand (#2) ──────────────────────

	$effect(() => {
		// eslint-disable-next-line @typescript-eslint/no-unused-expressions
		collapsed;
		flyoutServerId = null;
	});

	// ── Derived ─────────────────────────────────────────────────────

	function extractHost(wsUrl: string): string {
		try {
			const u = new URL(wsUrl.replace(/^ws/, 'http'));
			return u.host;
		} catch {
			return wsUrl;
		}
	}

	let displayServers: ServerDisplay[] = $derived(
		servers.map((s) => {
			const sessions = getServerDisplaySessions(s.id);
			return {
				id: s.id,
				name: s.name,
				host: extractHost(s.wsUrl),
				status: connectionStatuses[s.id] ?? 'disconnected',
				sessionCount: sessions.length,
				isActive: s.id === activeServerId
			};
		})
	);

	function getServerDisplaySessions(serverId: string): SessionDisplay[] {
		const local = serverSessions[serverId] ?? [];
		const remote = serverRemoteSessions[serverId] ?? [];

		const localDisplay: SessionDisplay[] = local.map((s) => ({
			id: s.sessionId,
			serverId,
			label: s.label || shortId(s.sessionId),
			pid: s.pid,
			status: s.attached ? 'attached' as const : 'open' as const,
			isActive: serverId === activeServerId && s.sessionId === activeSessionId
		}));

		const unattached: SessionDisplay[] = remote
			.filter((rs) => !local.some((s) => s.sessionId === rs.session_id))
			.map((rs) => ({
				id: rs.session_id,
				serverId,
				label: rs.name || shortId(rs.session_id),
				pid: rs.pid,
				status: 'detached' as const,
				isActive: false,
				sessionStatus: rs.status,
				idle: rs.idle,
				exitCode: rs.exit_code,
				idleTimeout: rs.idle_timeout
			}));

		return [...localDisplay, ...unattached];
	}

	let flyoutServer = $derived(
		flyoutServerId ? displayServers.find((s) => s.id === flyoutServerId) ?? null : null
	);

	function shortId(id: string): string {
		return id.slice(0, 8);
	}

	// ── Accordion ────────────────────────────────────────────────────

	function toggleAccordion(serverId: string) {
		if (shellPickerServerId === serverId) closeShellPicker();
		expandedServers = { ...expandedServers, [serverId]: !expandedServers[serverId] };
	}

	function handleServerRowClick(server: ServerDisplay) {
		if (collapsed) return;
		toggleAccordion(server.id);
	}

	// ── Connection toggle ────────────────────────────────────────────

	function handleDotClick(e: MouseEvent, server: ServerDisplay) {
		e.stopPropagation();
		if (server.status === 'disconnected') {
			onconnect?.(server.id);
		} else {
			// connected, connecting, or reconnecting — disconnect/abort
			ondisconnect?.(server.id);
		}
	}

	// ── Shell picker ────────────────────────────────────────────────

	function closeShellPicker() {
		if (shellPickerDismissTimer) { clearTimeout(shellPickerDismissTimer); shellPickerDismissTimer = null; }
		shellPickerServerId = null;
		shellPickerShells = [];
		shellPickerDefault = '';
		shellPickerLoading = false;
	}

	function scheduleShellPickerDismiss() {
		if (shellPickerDismissTimer) clearTimeout(shellPickerDismissTimer);
		shellPickerDismissTimer = setTimeout(() => {
			shellPickerDismissTimer = null;
			closeShellPicker();
		}, 2000);
	}

	function cancelShellPickerDismiss() {
		if (shellPickerDismissTimer) { clearTimeout(shellPickerDismissTimer); shellPickerDismissTimer = null; }
	}

	function fetchShellsForServer(serverId: string) {
		if (!onlistshells || shellFetching.has(serverId)) return;
		shellFetching.add(serverId);
		onlistshells(serverId).then((result) => {
			shellCache.set(serverId, result);
			shellFetching.delete(serverId);
			// If picker is waiting for this server, populate it
			if (shellPickerServerId === serverId && shellPickerLoading) {
				shellPickerShells = result.shells;
				shellPickerDefault = getShellDefault(serverId, result.defaultShell);
				shellPickerLoading = false;
			}
		}).catch(() => {
			shellFetching.delete(serverId);
			// If picker was waiting, close and start session directly
			if (shellPickerServerId === serverId && shellPickerLoading) {
				closeShellPicker();
				onnewsession?.(serverId);
			}
		});
	}

	function getShellDefault(serverId: string, serverDefault: string): string {
		const server = servers.find(s => s.id === serverId);
		return server?.shell || serverDefault;
	}

	/** Reorder shells so the default/last-used is first, rest in server order (elite rank). */
	function orderedShells(shells: string[], defaultShell: string): string[] {
		if (!defaultShell) return shells;
		const idx = shells.indexOf(defaultShell);
		if (idx <= 0) return shells;
		return [shells[idx], ...shells.slice(0, idx), ...shells.slice(idx + 1)];
	}

	function handleNewSessionClick(serverId: string, inFlyout: boolean) {
		if (inFlyout) flyoutServerId = null;
		const status = connectionStatuses[serverId] ?? 'disconnected';

		if (shellPickerServerId === serverId) {
			// Picker already open — use highlighted default and close
			const shell = shellPickerDefault || undefined;
			closeShellPicker();
			onnewsession?.(serverId, shell);
			return;
		}

		const cached = shellCache.get(serverId);
		if (cached) {
			// Shells already fetched — show picker instantly
			shellPickerServerId = serverId;
			shellPickerShells = cached.shells;
			shellPickerDefault = getShellDefault(serverId, cached.defaultShell);
			shellPickerLoading = false;
		} else if (status === 'connected') {
			// Connected but cache not ready yet — show loading, fetch now
			shellPickerServerId = serverId;
			shellPickerLoading = true;
			fetchShellsForServer(serverId);
		} else {
			// Disconnected — connect first, show loading spinner.
			// The auto-fetch $effect fires when status becomes 'connected',
			// and fetchShellsForServer populates the picker when it resolves.
			shellPickerServerId = serverId;
			shellPickerLoading = true;
			onconnect?.(serverId);
		}
	}

	function handleShellPick(serverId: string, shell: string) {
		closeShellPicker();
		oneditserver?.(serverId, { shell });
		onnewsession?.(serverId, shell);
	}

	// ── Session actions ──────────────────────────────────────────────

	function handleSessionClick(session: SessionDisplay) {
		if (session.status === 'attached' || session.status === 'open') {
			// Has a tab — just switch to it
			onselectsession?.(session.serverId, session.id);
		} else {
			// Remote-only — open tab with history (readonly, not attached)
			onopensession?.(session.serverId, session.id);
		}
	}

	// ── Remove confirmation (#3) ─────────────────────────────────────

	function handleRemoveClick(e: MouseEvent, serverId: string) {
		e.stopPropagation();
		if (confirmingRemoveId === serverId) {
			confirmingRemoveId = null;
			onremoveserver?.(serverId);
		} else {
			confirmingRemoveId = serverId;
		}
	}

	function handleKillClick(e: MouseEvent, serverId: string, sessionId: string) {
		e.stopPropagation();
		const key = `${serverId}/${sessionId}`;
		if (confirmingKillId === key) {
			confirmingKillId = null;
			onkillsession?.(serverId, sessionId);
		} else {
			confirmingKillId = key;
		}
	}

	// ── Flyout (collapsed mode) ──────────────────────────────────────

	function handleMouseEnter(serverId: string, event: MouseEvent) {
		if (!collapsed) return;
		if (hoverTimeout) clearTimeout(hoverTimeout);
		const el = event.currentTarget as HTMLElement;
		const rect = el.getBoundingClientRect();
		flyoutTop = rect.top;
		flyoutServerId = serverId;
	}

	function handleMouseLeave() {
		if (!collapsed) return;
		hoverTimeout = setTimeout(() => {
			flyoutServerId = null;
			closeShellPicker();
		}, 50);
	}

	function keepFlyoutOpen() {
		if (hoverTimeout) clearTimeout(hoverTimeout);
	}

	// ── Add server form ──────────────────────────────────────────────

	function openAddForm() {
		showAddForm = true;
		addName = '';
		addWsUrl = 'ws://localhost:1337/api/ws';
		addApiKey = '';
		addShell = '';
	}

	function submitAddForm() {
		if (!addName.trim() || !addWsUrl.trim()) return;
		onaddserver?.({ name: addName.trim(), wsUrl: addWsUrl.trim(), apiKey: addApiKey, shell: addShell });
		showAddForm = false;
	}

	function cancelAddForm() {
		showAddForm = false;
	}

	// ── Keyboard handling for forms (#4) ─────────────────────────────

	function handleAddFormKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') { e.preventDefault(); submitAddForm(); }
		else if (e.key === 'Escape') { e.preventDefault(); cancelAddForm(); }
	}

	function handleEditFormKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') { e.preventDefault(); submitEdit(); }
		else if (e.key === 'Escape') { e.preventDefault(); cancelEdit(); }
	}

	// ── Edit server ──────────────────────────────────────────────────

	function startEdit(serverId: string) {
		closeShellPicker();
		const server = servers.find((s) => s.id === serverId);
		if (!server) return;
		editingServerId = serverId;
		editName = server.name;
		editWsUrl = server.wsUrl;
		editApiKey = server.apiKey;
		editShell = server.shell;
	}

	function submitEdit() {
		if (!editingServerId || !editName.trim() || !editWsUrl.trim()) return;
		oneditserver?.(editingServerId, {
			name: editName.trim(),
			wsUrl: editWsUrl.trim(),
			apiKey: editApiKey,
			shell: editShell
		});
		editingServerId = null;
	}

	function cancelEdit() {
		editingServerId = null;
	}

	// ── Status helpers ──────────────────────────────────────────────

	function dotColor(status: ConnectionStatus): string {
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

	function dotTitle(status: ConnectionStatus): string {
		switch (status) {
			case 'connected':
				return 'Connected (click to disconnect)';
			case 'connecting':
				return 'Connecting... (click to abort)';
			case 'reconnecting':
				return 'Reconnecting... (click to abort)';
			default:
				return 'Disconnected (click to connect)';
		}
	}

	function statusLabel(status: ConnectionStatus): string | null {
		return null;
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="server-panel select-none" class:collapsed>
	<!-- Server rows -->
	{#each displayServers as server (server.id)}
		{@const isExpanded = !collapsed && !!expandedServers[server.id]}
		{@const sessions = getServerDisplaySessions(server.id)}

		<div
			class="server-row flex items-center h-8 cursor-pointer transition-colors
				{server.isActive ? 'bg-neutral-800/40' : 'hover:bg-neutral-800/40'}
				{flyoutServerId === server.id ? 'bg-neutral-800/40' : ''}"
			onmouseenter={(e) => handleMouseEnter(server.id, e)}
			onmouseleave={handleMouseLeave}
			onclick={() => handleServerRowClick(server)}
		>
			<!-- Connection dot (clickable) -->
			<div class="shrink-0 flex items-center justify-center" style="width: {collapsedWidth}px"
				onclick={(e: MouseEvent) => e.stopPropagation()}
			>
				<button
					class="w-2.5 h-2.5 rounded-full shrink-0 {dotColor(server.status)} transition-colors hover:ring-2 hover:ring-neutral-500/40"
					title={dotTitle(server.status)}
					onclick={(e) => handleDotClick(e, server)}
				></button>
			</div>
			<span class="server-label font-mono text-[11px] truncate whitespace-nowrap flex-1
				{server.isActive ? 'text-neutral-200' : 'text-neutral-400'}"
			>{server.name}</span>
			{#if !collapsed}
				<span class="server-label text-[10px] text-neutral-600 tabular-nums mr-1.5">
					{server.sessionCount > 0 ? server.sessionCount : ''}
				</span>
				<span class="server-label mr-2 select-none text-neutral-500">
					{#if isExpanded}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M19 9l-7 7-7-7" />
						</svg>
					{:else}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
						</svg>
					{/if}
				</span>
			{/if}
		</div>

		<!-- Expanded: sessions + actions -->
		{#if isExpanded}
			{@render serverBody(server, sessions, collapsedWidth, false)}

			<!-- Separator after expanded server -->
			<div class="border-b border-neutral-800/30"></div>
		{/if}
	{/each}

	<!-- Add server form (expanded mode, #4: keyboard handling) -->
	{#if !collapsed && showAddForm}
		<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
		<div
			class="px-2 py-2 bg-neutral-900/60 border-b border-neutral-800/50"
			style="padding-left: {collapsedWidth}px"
			onkeydown={handleAddFormKeydown}
		>
			<div class="flex flex-col gap-1.5">
				<input type="text" bind:value={addName} placeholder="name"
					class="w-full px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="text" bind:value={addWsUrl} placeholder="ws://host:port/api/ws"
					class="w-full px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="password" bind:value={addApiKey} placeholder="api key"
					class="w-full px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="text" bind:value={addShell} placeholder="shell (optional)"
					class="w-full px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<div class="flex items-center gap-1 pt-0.5">
					<button
						class="h-5 px-1.5 rounded text-[10px] bg-neutral-700 hover:bg-neutral-600 text-neutral-300 transition-colors inline-flex items-center gap-1"
						onclick={submitAddForm}
					>
						<svg class="w-3 h-3 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M5 13l4 4L19 7" />
						</svg>
						<span class="-translate-y-px">add</span>
					</button>
					<button
						class="h-5 px-1.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 transition-colors inline-flex items-center gap-1"
						onclick={cancelAddForm}
					>
						<svg class="w-3 h-3 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
						</svg>
						<span class="-translate-y-px">cancel</span>
					</button>
				</div>
			</div>
		</div>
	{/if}

	<!-- Add server button -->
		{#if !collapsed && !showAddForm}
			<div class="flex items-center justify-center py-2">
				<button
					class="w-8 h-8 flex items-center justify-center text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 rounded-full transition-colors"
					title="Add server"
					onclick={openAddForm}
				>
					<svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" />
					</svg>
				</button>
			</div>
		{/if}

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- Shared body snippet for accordion + flyout -->
{#snippet serverBody(server: ServerDisplay, sessions: SessionDisplay[], dotW: number, inFlyout: boolean)}
	<!-- Command bar: [+] [endpoint/shell picker] [gear] -->
	<div class="flex items-center h-6">
		<div class="flex items-center flex-1 min-w-0"
			onmouseenter={() => { if (shellPickerServerId === server.id) cancelShellPickerDismiss(); }}
			onmouseleave={() => { if (shellPickerServerId === server.id) scheduleShellPickerDismiss(); }}
		>
			<!-- New session — aligned with connection dots -->
			<div class="shrink-0 flex items-center justify-center" style="width: {dotW}px">
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors
						{shellPickerServerId === server.id
							? 'text-neutral-200 bg-neutral-700'
							: 'text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800'}"
					title={shellPickerServerId === server.id ? 'Start with default shell' : 'New session'}
					onclick={(e) => { e.stopPropagation(); handleNewSessionClick(server.id, inFlyout); }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" />
					</svg>
				</button>
			</div>
			<!-- Endpoint address or shell picker -->
			{#if shellPickerServerId === server.id}
				<!-- svelte-ignore a11y_no_static_element_interactions -->
			<div class="flex items-center gap-0.5 flex-1 overflow-x-auto scrollbar-none"
				onwheel={(e) => { if (e.deltaY) { e.preventDefault(); e.currentTarget.scrollLeft += e.deltaY; } }}
			>
					{#if shellPickerLoading}
						<span class="text-[10px] text-neutral-500 animate-pulse">detecting...</span>
					{:else}
						{#each orderedShells(shellPickerShells, shellPickerDefault) as shell}
							{@const name = shell.split('/').pop()}
							<button
								class="px-1.5 py-0.5 rounded text-[10px] font-mono transition-colors
									{shell === shellPickerDefault
										? 'bg-neutral-700 text-neutral-200'
										: 'text-neutral-500 hover:text-neutral-200 hover:bg-neutral-800'}"
								onclick={(e) => { e.stopPropagation(); handleShellPick(server.id, shell); }}
							>{name}</button>
						{/each}
					{/if}
				</div>
			{:else}
				<span class="font-mono text-[10px] text-neutral-500 truncate flex-1">{server.host}</span>
			{/if}
		</div>
		{#if !inFlyout}
			<!-- Device info toggle -->
			{#if server.status === 'connected' && serverDeviceInfo?.[server.id]}
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors
						{infoExpanded[server.id]
							? 'text-neutral-200 bg-neutral-800'
							: 'text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800'}"
					title="Device info"
					onclick={(e) => { e.stopPropagation(); infoExpanded = { ...infoExpanded, [server.id]: !infoExpanded[server.id] }; }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<rect x="2" y="3" width="20" height="14" rx="2" ry="2" />
						<path d="M8 21h8M12 17v4" />
					</svg>
				</button>
			{/if}
			<!-- Settings — aligned with kill buttons -->
			<button
				class="mr-1.5 w-5 h-5 flex items-center justify-center rounded transition-colors
					{editingServerId === server.id
						? 'text-neutral-200 bg-neutral-800'
						: 'text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800'}"
				title="Server settings"
				onclick={(e) => { e.stopPropagation(); if (editingServerId === server.id) cancelEdit(); else startEdit(server.id); }}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
					<path stroke-linecap="round" stroke-linejoin="round" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
				</svg>
			</button>
		{/if}
	</div>

	<!-- Settings panel (inline, under toolbar) -->
	{#if !inFlyout && editingServerId === server.id}
		<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
		<div
			class="py-1.5 pr-2 bg-neutral-900/60"
			style="padding-left: {dotW}px"
			onkeydown={handleEditFormKeydown}
		>
			<div class="flex flex-col gap-1">
				<input type="text" bind:value={editName} placeholder="name"
					class="w-full px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="text" bind:value={editWsUrl} placeholder="ws://host:port/api/ws"
					class="w-full px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="password" bind:value={editApiKey} placeholder="api key"
					class="w-full px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<input type="text" bind:value={editShell} placeholder="shell (optional)"
					class="w-full px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono focus:outline-none focus:border-neutral-500" />
				<div class="flex items-center gap-1 pt-0.5">
					<!-- Save -->
					<button
						class="h-5 px-1.5 rounded text-[10px] bg-neutral-700 hover:bg-neutral-600 text-neutral-300 transition-colors inline-flex items-center gap-1"
						onclick={submitEdit}
					>
						<svg class="w-3 h-3 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M5 13l4 4L19 7" />
						</svg>
						<span class="-translate-y-px">save</span>
					</button>
					<!-- Cancel -->
					<button
						class="h-5 px-1.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 transition-colors inline-flex items-center gap-1"
						onclick={cancelEdit}
					>
						<svg class="w-3 h-3 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
						</svg>
						<span class="-translate-y-px">cancel</span>
					</button>
					<div class="flex-1"></div>
					<!-- Nuke server -->
					<button
						class="h-5 w-16 rounded text-[10px] transition-colors inline-flex items-center justify-center gap-1
							{confirmingRemoveId === server.id
								? 'text-red-400 hover:text-red-300 hover:bg-red-900/30'
								: 'text-neutral-500 hover:text-red-400 hover:bg-neutral-800'}"
						title={confirmingRemoveId === server.id ? 'Click again to confirm' : 'Remove server permanently'}
						onclick={(e) => { handleRemoveClick(e, server.id); }}
						onmouseleave={() => { confirmingRemoveId = null; }}
					>
						<span class="-translate-y-px">{confirmingRemoveId === server.id ? '☢ boom?' : '☢ nuke'}</span>
					</button>
				</div>
			</div>
		</div>
	{/if}

	<!-- Device info panel -->
	{#if !inFlyout && infoExpanded[server.id] && serverDeviceInfo?.[server.id]}
		{@const devInfo = serverDeviceInfo[server.id]}
		{#if devInfo}
			<div class="border-b border-neutral-800/30">
				<DeviceInfoPanel info={devInfo} onrefresh={() => onrefreshinfo?.(server.id)} />
			</div>
		{/if}
	{/if}

	<!-- Activity feed -->
	{#if !inFlyout && server.status === 'connected' && (serverActivity?.[server.id]?.length ?? 0) > 0}
		{@const actEntries = serverActivity[server.id] ?? []}
		<div
			class="flex items-center h-5 cursor-pointer hover:bg-neutral-800/40 transition-colors"
			style="padding-left: {dotW}px"
			onclick={() => { activityExpanded = { ...activityExpanded, [server.id]: !activityExpanded[server.id] }; }}
		>
			<span class="text-neutral-500 mr-1">
				{#if activityExpanded[server.id]}
					<svg class="w-2.5 h-2.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M19 9l-7 7-7-7" />
					</svg>
				{:else}
					<svg class="w-2.5 h-2.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
					</svg>
				{/if}
			</span>
			<span class="text-[10px] text-neutral-500 font-mono">activity</span>
			<span class="ml-1 text-[9px] text-neutral-600 tabular-nums">{actEntries.length}</span>
		</div>
		{#if activityExpanded[server.id]}
			<div class="max-h-48 overflow-y-auto border-b border-neutral-800/30">
				<ActivityFeed entries={actEntries} />
			</div>
		{/if}
	{/if}

	<!-- Sessions -->
	{#each sessions as session (session.id)}
		<div
			class="group flex items-center h-7 cursor-pointer transition-colors hover:bg-neutral-800/40
				{session.isActive ? 'bg-neutral-800/60' : ''}"
			onclick={() => { if (inFlyout) flyoutServerId = null; handleSessionClick(session); }}
		>
			<div class="shrink-0 flex items-center justify-center" style="width: {dotW}px">
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors hover:bg-neutral-600/50"
					title={session.sessionStatus === 'exited'
						? `Exited (code ${session.exitCode ?? '?'})`
						: session.idleTimeout
							? `Auto-kill in ${session.idleTimeout}s`
							: session.status === 'attached' ? 'Detach session' : 'Attach session'}
					onclick={(e: MouseEvent) => { e.stopPropagation(); const serverId = session.serverId; const sessionId = session.id; if (inFlyout) flyoutServerId = null; if (session.status === 'attached') ondetachsession?.(serverId, sessionId); else onattachsession?.(serverId, sessionId); }}
				>
					<span class="w-1.5 h-1.5 rounded-full shrink-0
						{session.sessionStatus === 'exited' ? 'bg-neutral-500' : session.idle ? 'bg-neutral-600' : session.status === 'attached' ? 'bg-green-500' : 'bg-yellow-500'}
						{session.isActive ? 'ring-2 ring-green-400/40 ring-offset-1 ring-offset-neutral-950' : ''}"></span>
				</button>
			</div>
			<span class="font-mono text-[10px] truncate flex-1
				{session.sessionStatus === 'exited' ? 'text-neutral-600' : session.isActive ? 'text-neutral-200' : 'text-neutral-500'}"
			>{session.label}</span>
			{#if session.status !== 'attached'}
				<button
					class="mr-1.5 w-5 h-5 flex items-center justify-center rounded transition-all
						{confirmingKillId === `${session.serverId}/${session.id}`
							? 'opacity-100'
							: 'opacity-0 group-hover:opacity-100'}
						{confirmingKillId === `${session.serverId}/${session.id}`
							? 'text-red-500 hover:bg-red-900/30'
							: 'text-neutral-400 hover:text-yellow-500 hover:bg-neutral-600/50'}"
					title={confirmingKillId === `${session.serverId}/${session.id}` ? 'Click again to kill' : 'Kill session'}
					onclick={(e) => { handleKillClick(e, session.serverId, session.id); }}
					onmouseleave={() => { confirmingKillId = null; }}
				>
					<svg class="w-3 h-3" viewBox="0 0 16 16" fill="currentColor">
						<path d="M8 0C5.2 0 3 2.7 3 6c0 1.8.6 3.4 1.6 4.5L3 12h2.5l-.8 2H6l-.5 2h1L7 14h2l.5 2h1l-.5-2h1.3l-.8-2H13l-1.6-1.5C12.4 9.4 13 7.8 13 6c0-3.3-2.2-6-5-6zM5.5 5.5a1 1 0 112 0 1 1 0 01-2 0zm3 0a1 1 0 112 0 1 1 0 01-2 0z"/>
					</svg>
				</button>
			{/if}
		</div>
	{/each}
{/snippet}

<!-- Flyout (collapsed mode) -->
{#if collapsed && flyoutServerId && flyoutServer}
	{@const flyoutSessions = getServerDisplaySessions(flyoutServerId)}
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		class="fixed z-50 bg-neutral-900 border border-neutral-700 rounded-md shadow-xl"
		style="top: {flyoutTop}px; left: {collapsedWidth}px;"
		onmouseenter={keepFlyoutOpen}
		onmouseleave={handleMouseLeave}
	>
		<div class="min-w-[180px]">
			<!-- Header bar -->
			<div class="flex items-center gap-2 px-2.5 py-1.5 border-b border-neutral-700/50">
				<span class="text-xs text-neutral-300 font-mono truncate flex-1">{flyoutServer.name}</span>
				{#if flyoutServer.sessionCount > 0}
					<span class="text-[10px] text-neutral-600 tabular-nums">{flyoutServer.sessionCount}</span>
				{/if}
			</div>
			<!-- Shared body -->
			{@render serverBody(flyoutServer, flyoutSessions, 28, true)}
		</div>
	</div>
{/if}
</div>

<style>
	.server-panel .server-label {
		transition: opacity 0.2s ease-in-out;
	}
	.server-panel.collapsed .server-label {
		opacity: 0;
	}
</style>
