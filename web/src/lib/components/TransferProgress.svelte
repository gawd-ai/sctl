<script lang="ts">
	import type { ClientTransfer } from '../utils/transfer';

	interface Props {
		transfers: ClientTransfer[];
		onabort?: (transferId: string) => void;
		ondismiss?: (transferId: string) => void;
		class?: string;
	}

	let { transfers, onabort, ondismiss, class: className = '' }: Props = $props();

	const hasTransfers = $derived(transfers.length > 0);

	function formatRate(bps: number): string {
		if (bps < 1024) return `${bps} B/s`;
		if (bps < 1048576) return `${(bps / 1024).toFixed(0)} KB/s`;
		return `${(bps / 1048576).toFixed(1)} MB/s`;
	}

	function formatSize(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	function truncName(name: string, max = 24): string {
		if (name.length <= max) return name;
		const ext = name.lastIndexOf('.');
		if (ext > 0 && name.length - ext <= 8) {
			const keep = max - (name.length - ext) - 2;
			return name.slice(0, keep) + '..' + name.slice(ext);
		}
		return name.slice(0, max - 2) + '..';
	}
</script>

{#if hasTransfers}
<div class="font-mono text-[11px] {className}">
	<!-- Transfer list -->
	<div class="max-h-48 overflow-y-auto">
		{#each transfers as t (t.transferId)}
			<div class="px-2 py-1.5 border-b border-neutral-800 last:border-0">
				<!-- Row 1: direction + name + actions -->
				<div class="flex items-center gap-1">
					<span class="text-neutral-500 shrink-0" title={t.direction}>
						{t.direction === 'download' ? 'v' : '^'}
					</span>
					<span class="text-neutral-300 truncate flex-1" title={t.filename}>
						{truncName(t.filename)}
					</span>
					{#if t.state === 'active'}
						<button
							class="text-neutral-600 hover:text-red-400 transition-colors shrink-0"
							title="Cancel"
							onclick={() => onabort?.(t.transferId)}
						>
							<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
								<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
							</svg>
						</button>
					{:else}
						<button
							class="text-neutral-600 hover:text-neutral-300 transition-colors shrink-0"
							title="Dismiss"
							onclick={() => ondismiss?.(t.transferId)}
						>
							<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
								<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
							</svg>
						</button>
					{/if}
				</div>

				<!-- Row 2: progress bar + stats -->
				{#if t.state === 'active'}
					<div class="flex items-center gap-1.5 mt-1">
						<div class="flex-1 h-1 rounded-full bg-neutral-800 overflow-hidden">
							<div
								class="h-full rounded-full bg-blue-500 transition-all duration-300"
								style="width: {Math.round(t.progress.fraction * 100)}%"
							></div>
						</div>
						<span class="text-neutral-500 tabular-nums shrink-0">
							{Math.round(t.progress.fraction * 100)}%
						</span>
						<span class="text-neutral-600 tabular-nums shrink-0">
							{formatRate(t.progress.rateBps)}
						</span>
					</div>
				{:else if t.state === 'complete'}
					<div class="flex items-center gap-1 mt-0.5">
						<div class="flex-1 h-1 rounded-full bg-green-800">
							<div class="h-full rounded-full bg-green-500 w-full"></div>
						</div>
						<span class="text-green-400 text-[10px]">{formatSize(t.fileSize)}</span>
					</div>
				{:else if t.state === 'error'}
					<div class="mt-0.5 text-red-400 text-[10px] truncate" title={t.error}>
						{t.error}
					</div>
				{:else if t.state === 'aborted'}
					<div class="mt-0.5 text-neutral-500 text-[10px]">Cancelled</div>
				{/if}
			</div>
		{/each}
	</div>
</div>
{/if}
