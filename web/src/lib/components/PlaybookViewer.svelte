<script lang="ts">
	import type { PlaybookDetail } from '../types/terminal.types';

	interface Props {
		playbook: PlaybookDetail | null;
		onexecute?: (playbook: PlaybookDetail) => void;
		onedit?: (playbook: PlaybookDetail) => void;
		onclose?: () => void;
	}

	let { playbook, onexecute, onedit, onclose }: Props = $props();

	let paramEntries = $derived(
		playbook ? Object.entries(playbook.params).sort(([a], [b]) => a.localeCompare(b)) : []
	);
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-viewer flex flex-col h-full bg-neutral-900 font-mono">
	{#if playbook}
		<!-- Header -->
		<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800 shrink-0">
			<div class="flex-1 min-w-0">
				<div class="text-xs text-neutral-200 font-semibold truncate">{playbook.name}</div>
				<div class="text-[10px] text-neutral-500 truncate">{playbook.description}</div>
			</div>
			{#if onexecute}
				<button
					class="px-2 py-1 rounded text-[10px] bg-green-900/40 text-green-400 hover:bg-green-900/60 transition-colors"
					onclick={() => onexecute?.(playbook)}
				>Run</button>
			{/if}
			{#if onedit}
				<button
					class="px-2 py-1 rounded text-[10px] bg-neutral-800 text-neutral-400 hover:text-neutral-200 hover:bg-neutral-700 transition-colors"
					onclick={() => onedit?.(playbook)}
				>Edit</button>
			{/if}
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

		<div class="flex-1 overflow-y-auto min-h-0 px-3 py-2 space-y-3">
			<!-- Parameters -->
			{#if paramEntries.length > 0}
				<div>
					<div class="text-[10px] text-neutral-500 uppercase tracking-wide mb-1">Parameters</div>
					<div class="border border-neutral-800 rounded overflow-hidden">
						<table class="w-full text-[10px]">
							<thead>
								<tr class="bg-neutral-800/50">
									<th class="text-left px-2 py-1 text-neutral-500 font-normal">Name</th>
									<th class="text-left px-2 py-1 text-neutral-500 font-normal">Type</th>
									<th class="text-left px-2 py-1 text-neutral-500 font-normal">Description</th>
									<th class="text-left px-2 py-1 text-neutral-500 font-normal">Default</th>
								</tr>
							</thead>
							<tbody>
								{#each paramEntries as [name, param]}
									<tr class="border-t border-neutral-800/50">
										<td class="px-2 py-1 text-neutral-300">{name}</td>
										<td class="px-2 py-1 text-neutral-500">{param.type}</td>
										<td class="px-2 py-1 text-neutral-400">{param.description}</td>
										<td class="px-2 py-1 text-neutral-600">
											{param.default !== undefined ? String(param.default) : '-'}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				</div>
			{/if}

			<!-- Script -->
			<div>
				<div class="text-[10px] text-neutral-500 uppercase tracking-wide mb-1">Script</div>
				<pre class="p-2 bg-neutral-800/50 border border-neutral-800 rounded text-[10px] text-neutral-300 whitespace-pre-wrap break-all overflow-x-auto">{playbook.script}</pre>
			</div>
		</div>
	{:else}
		<div class="flex items-center justify-center h-full text-[10px] text-neutral-600">
			Select a playbook to view
		</div>
	{/if}
</div>
