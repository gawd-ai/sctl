<script lang="ts">
	import type { ActivityEntry, ActivityType, ActivitySource } from '../types/terminal.types';

	interface Props {
		entries: ActivityEntry[];
	}

	let { entries }: Props = $props();

	let expandedId: number | null = $state(null);

	function typeIcon(t: ActivityType): string {
		switch (t) {
			case 'exec':
				return '$';
			case 'file_read':
				return 'R';
			case 'file_write':
				return 'W';
			case 'file_list':
				return 'L';
			case 'session_start':
				return '+';
			case 'session_exec':
				return '>';
			case 'session_kill':
				return 'x';
			case 'session_signal':
				return '!';
			default:
				return '?';
		}
	}

	function typeColor(t: ActivityType): string {
		switch (t) {
			case 'exec':
			case 'session_exec':
				return 'text-neutral-400';
			case 'file_write':
				return 'text-yellow-500/70';
			case 'session_kill':
			case 'session_signal':
				return 'text-red-400/70';
			case 'session_start':
				return 'text-green-400/70';
			default:
				return 'text-neutral-500';
		}
	}

	function sourceColor(s: ActivitySource): string {
		switch (s) {
			case 'mcp':
				return 'bg-blue-500/20 text-blue-400';
			case 'ws':
				return 'bg-green-500/20 text-green-400';
			case 'rest':
				return 'bg-amber-500/20 text-amber-400';
			default:
				return 'bg-neutral-500/20 text-neutral-400';
		}
	}

	function relativeTime(timestampMs: number): string {
		const delta = Date.now() - timestampMs;
		if (delta < 1000) return 'now';
		if (delta < 60_000) return `${Math.floor(delta / 1000)}s`;
		if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m`;
		return `${Math.floor(delta / 3_600_000)}h`;
	}

	/** Format detail values for human display. */
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

	/** Color exit codes: 0=green, non-zero=red. */
	function exitCodeColor(value: unknown): string {
		if (typeof value !== 'number') return 'text-neutral-400';
		return value === 0 ? 'text-green-400/80' : 'text-red-400/80';
	}

	function toggleExpand(id: number) {
		expandedId = expandedId === id ? null : id;
	}

	// Show newest first
	let sorted = $derived([...entries].reverse());
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="activity-feed font-mono text-[10px]">
	{#each sorted as entry (entry.id)}
		<div
			class="flex items-start gap-1 px-1.5 py-0.5 hover:bg-neutral-800/40 cursor-pointer transition-colors"
			onclick={() => toggleExpand(entry.id)}
		>
			<!-- Type icon -->
			<span class="w-3 text-center shrink-0 {typeColor(entry.activity_type)}"
				>{typeIcon(entry.activity_type)}</span
			>
			<!-- Source badge -->
			<span class="shrink-0 px-1 rounded text-[9px] leading-[14px] {sourceColor(entry.source)}"
				>{entry.source}</span
			>
			<!-- Summary -->
			<span class="flex-1 truncate text-neutral-400">{entry.summary}</span>
			<!-- Expand indicator + relative time -->
			{#if entry.detail}
				<span class="shrink-0 text-neutral-700 text-[8px]"
					>{expandedId === entry.id ? '▾' : '▸'}</span
				>
			{/if}
			<span class="shrink-0 text-neutral-600 tabular-nums">{relativeTime(entry.timestamp)}</span>
		</div>
		<!-- Expanded detail -->
		{#if expandedId === entry.id && entry.detail}
			<div class="px-1.5 py-1 ml-4 mb-0.5 bg-neutral-800/30 rounded text-[9px] text-neutral-500">
				{#each Object.entries(entry.detail) as [key, value]}
					{#if value !== null && value !== undefined && value !== ''}
						<div class="flex gap-1">
							<span class="text-neutral-600 shrink-0">{key}:</span>
							<span
								class="break-all {key === 'exit_code'
									? exitCodeColor(value)
									: 'text-neutral-400'}">{formatValue(key, value)}</span
							>
						</div>
					{/if}
				{/each}
			</div>
		{/if}
	{/each}
	{#if entries.length === 0}
		<div class="px-1.5 py-2 text-neutral-600 text-center">no activity</div>
	{/if}
</div>
