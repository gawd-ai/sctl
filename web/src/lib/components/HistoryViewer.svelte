<script lang="ts">
	import type { ActivityEntry, ActivityType, ActivitySource, ViewerTab } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';

	interface Props {
		entries: ActivityEntry[];
		restClient: SctlRestClient | null;
		onloadmore?: () => void;
		onclose?: () => void;
		onOpenViewer?: (tab: ViewerTab) => void;
	}

	let { entries, restClient, onloadmore, onclose, onOpenViewer }: Props = $props();

	let loadingResult: number | null = $state(null);

	// ── Filter state ────────────────────────────────────────────────
	let activeTypes: Set<ActivityType> = $state(new Set());
	let activeSources: Set<ActivitySource> = $state(new Set());
	let searchQuery = $state('');
	let expandedIds: Set<number> = $state(new Set());

	const allTypes: ActivityType[] = [
		'exec', 'file_read', 'file_write', 'file_list',
		'session_start', 'session_exec', 'session_kill', 'session_signal',
		'playbook_list', 'playbook_read', 'playbook_write', 'playbook_delete'
	];
	const allSources: ActivitySource[] = ['mcp', 'ws', 'rest'];

	// ── Filtering ───────────────────────────────────────────────────

	let filtered = $derived((() => {
		let result = [...entries].reverse();
		if (activeTypes.size > 0) {
			result = result.filter(e => activeTypes.has(e.activity_type));
		}
		if (activeSources.size > 0) {
			result = result.filter(e => activeSources.has(e.source));
		}
		if (searchQuery.trim()) {
			const q = searchQuery.toLowerCase();
			result = result.filter(e =>
				e.summary.toLowerCase().includes(q) ||
				(e.detail && JSON.stringify(e.detail).toLowerCase().includes(q))
			);
		}
		return result;
	})());

	// ── Toggle helpers ──────────────────────────────────────────────

	function toggleType(t: ActivityType) {
		const next = new Set(activeTypes);
		if (next.has(t)) next.delete(t);
		else next.add(t);
		activeTypes = next;
	}

	function toggleSource(s: ActivitySource) {
		const next = new Set(activeSources);
		if (next.has(s)) next.delete(s);
		else next.add(s);
		activeSources = next;
	}

	function toggleExpand(id: number) {
		const next = new Set(expandedIds);
		if (next.has(id)) next.delete(id);
		else next.add(id);
		expandedIds = next;
	}

	function clearFilters() {
		activeTypes = new Set();
		activeSources = new Set();
		searchQuery = '';
	}

	// ── Display helpers ─────────────────────────────────────────────

	function typeIcon(t: ActivityType): string {
		switch (t) {
			case 'exec': return '$';
			case 'file_read': return 'R';
			case 'file_write': return 'W';
			case 'file_list': return 'L';
			case 'session_start': return '+';
			case 'session_exec': return '>';
			case 'session_kill': return 'x';
			case 'session_signal': return '!';
			case 'playbook_list': return 'P';
			case 'playbook_read': return 'P';
			case 'playbook_write': return 'P';
			case 'playbook_delete': return 'P';
			default: return '?';
		}
	}

	function typeColor(t: ActivityType): string {
		switch (t) {
			case 'exec':
			case 'session_exec':
				return 'text-neutral-400';
			case 'file_write':
			case 'playbook_write':
				return 'text-yellow-500/70';
			case 'session_kill':
			case 'session_signal':
			case 'playbook_delete':
				return 'text-red-400/70';
			case 'session_start':
				return 'text-green-400/70';
			case 'playbook_list':
			case 'playbook_read':
				return 'text-purple-400/70';
			default:
				return 'text-neutral-500';
		}
	}

	function sourceColor(s: ActivitySource): string {
		switch (s) {
			case 'mcp': return 'bg-blue-500/20 text-blue-400';
			case 'ws': return 'bg-green-500/20 text-green-400';
			case 'rest': return 'bg-amber-500/20 text-amber-400';
			default: return 'bg-neutral-500/20 text-neutral-400';
		}
	}

	function typeLabel(t: ActivityType): string {
		return t.replace(/_/g, ' ');
	}

	function relativeTime(timestampMs: number): string {
		const delta = Date.now() - timestampMs;
		if (delta < 1000) return 'now';
		if (delta < 60_000) return `${Math.floor(delta / 1000)}s`;
		if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m`;
		return `${Math.floor(delta / 3_600_000)}h`;
	}

	function formatValue(key: string, value: unknown): string {
		if (value === null || value === undefined) return '';
		if (key === 'duration_ms' && typeof value === 'number') {
			return value < 1000 ? `${value}ms` : `${(value / 1000).toFixed(1)}s`;
		}
		if (key === 'size' && typeof value === 'number') {
			if (value < 1024) return `${value} B`;
			if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
			return `${(value / (1024 * 1024)).toFixed(1)} MB`;
		}
		if (typeof value === 'string') return value;
		return JSON.stringify(value);
	}

	function exitCodeColor(value: unknown): string {
		if (typeof value !== 'number') return 'text-neutral-400';
		return value === 0 ? 'text-green-400/80' : 'text-red-400/80';
	}

	async function openExecResult(entry: ActivityEntry) {
		if (!restClient || !onOpenViewer) return;
		loadingResult = entry.id;
		try {
			const result = await restClient.getExecResult(entry.id);
			const tab: ViewerTab = {
				key: crypto.randomUUID(),
				type: 'exec',
				label: result.command.length > 24
					? result.command.slice(0, 24) + '...'
					: result.command,
				icon: '$',
				data: {
					activityId: result.activity_id,
					command: result.command,
					exitCode: result.exit_code,
					stdout: result.stdout,
					stderr: result.stderr,
					durationMs: result.duration_ms,
					status: result.status,
					errorMessage: result.error_message,
				}
			};
			onOpenViewer(tab);
		} catch (err) {
			console.error('Failed to fetch exec result:', err);
		} finally {
			loadingResult = null;
		}
	}

	let hasActiveFilters = $derived(activeTypes.size > 0 || activeSources.size > 0 || searchQuery.trim() !== '');
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="history-viewer flex flex-col h-full bg-neutral-950 font-mono text-[11px]">
	<!-- Header -->
	<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800 shrink-0">
		<span class="text-neutral-300 text-xs font-semibold">Activity History</span>
		<span class="text-[9px] text-neutral-600 tabular-nums">{filtered.length} / {entries.length}</span>
		<div class="flex-1"></div>
		<button
			class="px-1.5 py-0.5 rounded text-[9px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
			style:visibility={hasActiveFilters ? 'visible' : 'hidden'}
			onclick={clearFilters}
		>clear</button>
		{#if onclose}
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
				onclick={onclose}
			>
				<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
				</svg>
			</button>
		{/if}
	</div>

	<!-- Filter bar: source chips first, then type chips -->
	<div class="flex flex-wrap gap-1 px-3 py-1.5 border-b border-neutral-800/50 shrink-0">
		{#each allSources as s}
			<button
				class="px-1.5 py-0.5 rounded text-[9px] transition-colors
					{activeSources.has(s)
						? sourceColor(s)
						: 'text-neutral-600 hover:text-neutral-400 hover:bg-neutral-800'}"
				onclick={() => toggleSource(s)}
			>{s}</button>
		{/each}
		<span class="w-px h-4 bg-neutral-800 mx-0.5"></span>
		{#each allTypes as t}
			<button
				class="px-1.5 py-0.5 rounded text-[9px] transition-colors
					{activeTypes.has(t)
						? 'bg-neutral-700 text-neutral-200'
						: 'text-neutral-600 hover:text-neutral-400 hover:bg-neutral-800'}"
				onclick={() => toggleType(t)}
			>{typeLabel(t)}</button>
		{/each}
	</div>

	<!-- Search bar -->
	<div class="px-3 py-1.5 border-b border-neutral-800/50 shrink-0">
		<input
			type="text"
			bind:value={searchQuery}
			placeholder="Search activity..."
			class="w-full px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-300 placeholder-neutral-600 focus:outline-none focus:border-neutral-500"
		/>
	</div>

	<!-- Entries list -->
	<div class="flex-1 overflow-y-auto min-h-0">
		{#each filtered as entry (entry.id)}
			<div
				class="flex items-start gap-1 px-3 py-1 hover:bg-neutral-800/40 cursor-pointer transition-colors border-b border-neutral-800/20"
				onclick={() => toggleExpand(entry.id)}
			>
				<!-- Type icon -->
				<span class="w-3 text-center shrink-0 {typeColor(entry.activity_type)}"
					>{typeIcon(entry.activity_type)}</span>
				<!-- Source badge -->
				<span class="shrink-0 px-1 rounded text-[9px] leading-[14px] {sourceColor(entry.source)}"
					>{entry.source}</span>
				<!-- Summary -->
				<span class="flex-1 truncate text-neutral-400">{entry.summary}</span>
				<!-- Expand indicator -->
				{#if entry.detail}
					<span class="shrink-0 text-neutral-700 text-[8px]"
						>{expandedIds.has(entry.id) ? '▾' : '▸'}</span>
				{/if}
				<!-- Time -->
				<span class="shrink-0 text-neutral-600 tabular-nums text-[10px]">{relativeTime(entry.timestamp)}</span>
			</div>
			<!-- Expanded detail -->
			{#if expandedIds.has(entry.id) && entry.detail}
				<div class="px-3 py-1.5 ml-5 mb-0.5 bg-neutral-800/30 rounded text-[9px] text-neutral-500 border-b border-neutral-800/20">
					{#each Object.entries(entry.detail) as [key, value]}
						{#if value !== null && value !== undefined && value !== '' && key !== 'has_full_output'}
							<div class="flex gap-1">
								<span class="text-neutral-600 shrink-0">{key}:</span>
								<span class="break-all {key === 'exit_code' ? exitCodeColor(value) : 'text-neutral-400'}"
									>{formatValue(key, value)}</span>
							</div>
						{/if}
					{/each}
					{#if entry.detail.has_full_output && onOpenViewer}
						<button
							class="mt-1 px-2 py-0.5 rounded text-[9px] bg-blue-500/15 text-blue-400 hover:bg-blue-500/25 transition-colors disabled:opacity-50"
							disabled={loadingResult === entry.id}
							onclick={(e) => { e.stopPropagation(); openExecResult(entry); }}
						>{loadingResult === entry.id ? 'loading...' : 'view full output'}</button>
					{/if}
				</div>
			{/if}
		{/each}
		{#if filtered.length === 0}
			<div class="flex items-center justify-center py-8 text-neutral-600">
				{hasActiveFilters ? 'No matching activity' : 'No activity recorded'}
			</div>
		{/if}
	</div>

	<!-- Footer: load more -->
	{#if onloadmore && entries.length > 0}
		<div class="flex items-center justify-center py-2 border-t border-neutral-800 shrink-0">
			<button
				class="px-3 py-1 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
				onclick={onloadmore}
			>Load more</button>
		</div>
	{/if}
</div>
