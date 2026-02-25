<script lang="ts">
	import type { ClientTransfer } from '../utils/transfer';
	import TransferProgress from './TransferProgress.svelte';

	interface Props {
		transfers: ClientTransfer[];
		onabort?: (transferId: string) => void;
		ondismiss?: (transferId: string) => void;
	}

	let { transfers, onabort, ondismiss }: Props = $props();

	let dropdownOpen = $state(false);
	let containerEl: HTMLDivElement | undefined = $state();

	const hasTransfers = $derived(transfers.length > 0);
	const activeCount = $derived(transfers.filter((t) => t.state === 'active').length);

	function toggleDropdown() {
		dropdownOpen = !dropdownOpen;
	}

	function handleClickOutside(e: MouseEvent) {
		if (containerEl && !containerEl.contains(e.target as Node)) {
			dropdownOpen = false;
		}
	}

	$effect(() => {
		if (dropdownOpen) {
			document.addEventListener('click', handleClickOutside, true);
			return () => document.removeEventListener('click', handleClickOutside, true);
		}
	});
</script>

{#if hasTransfers}
	<div class="relative shrink-0" bind:this={containerEl}>
		<button
			class="flex items-center gap-1 h-8 px-2 text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800 transition-colors"
			onclick={toggleDropdown}
			title="{transfers.length} transfer{transfers.length !== 1 ? 's' : ''}"
		>
			<!-- Arrow icon -->
			<svg class="w-3.5 h-3.5 {activeCount > 0 ? 'text-blue-400 animate-pulse' : ''}" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M7 16V4m0 0L3 8m4-4l4 4m6 0v12m0 0l4-4m-4 4l-4-4" />
			</svg>
			<span class="text-[9px] tabular-nums font-mono">{transfers.length}</span>
		</button>

		<!-- Dropdown -->
		{#if dropdownOpen}
			<div class="absolute right-0 top-full mt-1 z-50 w-72 rounded border border-neutral-700 bg-neutral-900 shadow-xl overflow-hidden font-mono text-[11px]">
				<TransferProgress
					{transfers}
					{onabort}
					{ondismiss}
				/>
			</div>
		{/if}
	</div>
{/if}
