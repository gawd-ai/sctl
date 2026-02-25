<script lang="ts">
	import type { PlaybookSummary } from '../types/terminal.types';

	interface Props {
		playbooks: PlaybookSummary[];
		onselect?: (name: string) => void;
		ondelete?: (name: string) => void;
		oncreate?: () => void;
		onrefresh?: () => void;
	}

	let { playbooks, onselect, ondelete, oncreate, onrefresh }: Props = $props();

	let confirmingDelete: string | null = $state(null);

	function handleDelete(e: MouseEvent, name: string) {
		e.stopPropagation();
		if (confirmingDelete === name) {
			confirmingDelete = null;
			ondelete?.(name);
		} else {
			confirmingDelete = name;
		}
	}

	let sorted = $derived([...playbooks].sort((a, b) => a.name.localeCompare(b.name)));
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-list flex flex-col h-full">
	<!-- Header -->
	<div class="flex items-center gap-1 px-1.5 h-8 border-b border-neutral-700 shrink-0">
		<span class="text-[11px] text-neutral-300 font-mono font-semibold flex-1">Playbooks</span>
		<span class="text-[9px] text-neutral-600 tabular-nums">{playbooks.length}</span>
		{#if onrefresh}
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
				title="Refresh"
				onclick={onrefresh}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
				</svg>
			</button>
		{/if}
		{#if oncreate}
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
				title="New playbook"
				onclick={oncreate}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" />
				</svg>
			</button>
		{/if}
	</div>

	<!-- List -->
	<div class="flex-1 overflow-y-auto min-h-0">
		{#each sorted as pb (pb.name)}
			<div
				class="group flex items-center gap-2 px-2 py-1.5 hover:bg-neutral-800/40 cursor-pointer transition-colors border-b border-neutral-800/20"
				onclick={() => onselect?.(pb.name)}
			>
				<div class="flex-1 min-w-0">
					<div class="text-[11px] font-mono text-neutral-300 truncate">{pb.name}</div>
					<div class="text-[9px] text-neutral-500 truncate">{pb.description}</div>
				</div>
				{#if pb.params.length > 0}
					<span class="shrink-0 px-1 rounded text-[9px] bg-neutral-800 text-neutral-500 tabular-nums">
						{pb.params.length}
					</span>
				{/if}
				{#if ondelete}
					<button
						class="shrink-0 w-5 h-5 flex items-center justify-center rounded transition-all
							{confirmingDelete === pb.name
								? 'opacity-100 text-red-500 hover:bg-red-900/30'
								: 'opacity-0 group-hover:opacity-100 text-neutral-500 hover:text-red-400 hover:bg-neutral-700'}"
						title={confirmingDelete === pb.name ? 'Click again to confirm' : 'Delete playbook'}
						onclick={(e) => handleDelete(e, pb.name)}
						onmouseleave={() => { confirmingDelete = null; }}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
						</svg>
					</button>
				{/if}
			</div>
		{/each}
		{#if playbooks.length === 0}
			<div class="flex items-center justify-center py-8 text-[10px] text-neutral-600">No playbooks</div>
		{/if}
	</div>
</div>
