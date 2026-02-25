<script lang="ts">
	import type { ExecViewerData } from '../types/terminal.types';

	interface Props {
		data: ExecViewerData;
		onclose?: () => void;
	}

	let { data, onclose }: Props = $props();

	function formatDuration(ms: number): string {
		if (ms < 1000) return `${ms}ms`;
		if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
		return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1000)}s`;
	}

	function exitCodeClass(code: number): string {
		if (code === 0) return 'text-green-400';
		return 'text-red-400';
	}

	function statusBadgeClass(status: string): string {
		switch (status) {
			case 'ok': return 'bg-green-500/20 text-green-400';
			case 'timeout': return 'bg-amber-500/20 text-amber-400';
			case 'error': return 'bg-red-500/20 text-red-400';
			default: return 'bg-neutral-500/20 text-neutral-400';
		}
	}

	let showStderr = $state(true);
</script>

<div class="flex flex-col h-full bg-neutral-950 font-mono text-[11px]">
	<!-- Header -->
	<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800 shrink-0">
		<span class="text-neutral-500">$</span>
		<span class="flex-1 truncate text-neutral-300" title={data.command}>{data.command}</span>
		<span class="px-1.5 py-0.5 rounded text-[9px] {statusBadgeClass(data.status)}">{data.status}</span>
		<span class="{exitCodeClass(data.exitCode)} text-[10px] tabular-nums">
			exit {data.exitCode}
		</span>
		<span class="text-neutral-600 text-[10px] tabular-nums">{formatDuration(data.durationMs)}</span>
		{#if onclose}
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
				onclick={onclose}
				aria-label="Close viewer"
			>
				<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
				</svg>
			</button>
		{/if}
	</div>

	<!-- Error message banner -->
	{#if data.errorMessage}
		<div class="px-3 py-1.5 bg-red-500/10 border-b border-red-500/20 text-red-400 text-[10px]">
			{data.errorMessage}
		</div>
	{/if}

	<!-- Stderr toggle (only if stderr present) -->
	{#if data.stderr}
		<div class="flex items-center gap-2 px-3 py-1 border-b border-neutral-800/50 shrink-0">
			<button
				class="px-1.5 py-0.5 rounded text-[9px] transition-colors
					{showStderr ? 'bg-red-500/20 text-red-400' : 'text-neutral-600 hover:text-neutral-400 hover:bg-neutral-800'}"
				onclick={() => { showStderr = !showStderr; }}
			>stderr ({data.stderr.split('\n').length} lines)</button>
		</div>
	{/if}

	<!-- Content area -->
	<div class="flex-1 overflow-auto min-h-0">
		<!-- Stderr block -->
		{#if data.stderr && showStderr}
			<div class="border-b border-red-500/10">
				<div class="px-3 py-0.5 text-[9px] text-red-400/60 bg-red-500/5 sticky top-0">stderr</div>
				<pre class="px-3 py-2 text-red-300/80 whitespace-pre-wrap break-all text-[11px] leading-[16px] select-text">{data.stderr}</pre>
			</div>
		{/if}

		<!-- Stdout block -->
		{#if data.stdout}
			<div>
				{#if data.stderr && showStderr}
					<div class="px-3 py-0.5 text-[9px] text-neutral-600 bg-neutral-800/30 sticky top-0">stdout</div>
				{/if}
				<pre class="px-3 py-2 text-neutral-300 whitespace-pre-wrap break-all text-[11px] leading-[16px] select-text">{data.stdout}</pre>
			</div>
		{:else if !data.stderr}
			<div class="flex items-center justify-center py-12 text-neutral-600">
				No output
			</div>
		{/if}
	</div>
</div>
