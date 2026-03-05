<script lang="ts">
	import type { PlaybookDetail } from '../types/terminal.types';

	interface Props {
		playbook: PlaybookDetail | null;
		onedit?: (playbook: PlaybookDetail) => void;
		onexecute?: (playbook: PlaybookDetail) => void;
		onclose?: () => void;
	}

	let { playbook, onedit, onexecute, onclose }: Props = $props();

	let paramEntries = $derived(
		playbook ? Object.entries(playbook.params).sort(([a], [b]) => a.localeCompare(b)) : []
	);

	let scriptExpanded = $state(false);
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-viewer flex flex-col h-full bg-neutral-900 font-mono">
	{#if playbook}
		<div class="flex-1 overflow-y-auto min-h-0 px-3 py-2 space-y-3">
			<!-- Description -->
			<div class="text-[10px] text-neutral-500">{playbook.description}</div>
			<!-- Parameters -->
			{#if paramEntries.length > 0}
				<div>
					<div class="text-[10px] text-neutral-500 uppercase tracking-wide mb-1">Parameters</div>
					<div class="space-y-1.5">
						{#each paramEntries as [name, param]}
							<div class="px-2 py-1 bg-neutral-800/30 rounded border border-neutral-800/50">
								<div class="flex items-baseline gap-1.5">
									<span class="text-[10px] text-neutral-300 font-semibold">{name}</span>
									<span class="text-[9px] text-neutral-600">{param.type}</span>
									{#if param.default !== undefined && String(param.default) !== ''}
										<span class="text-[9px] text-neutral-600 ml-auto">= {String(param.default)}</span>
									{/if}
								</div>
								{#if param.description}
									<div class="text-[9px] text-neutral-500 mt-0.5">{param.description}</div>
								{/if}
								{#if param.enum}
									<div class="flex flex-wrap gap-1 mt-1">
										{#each param.enum as val}
											<span class="px-1 rounded text-[8px] bg-neutral-800 text-neutral-500">{val}</span>
										{/each}
									</div>
								{/if}
							</div>
						{/each}
					</div>
				</div>
			{/if}

			<!-- Script (collapsible) -->
			<div>
				<button
					class="flex items-center gap-1 text-[10px] text-neutral-500 uppercase tracking-wide mb-1 hover:text-neutral-400 transition-colors"
					onclick={() => { scriptExpanded = !scriptExpanded; }}
				>
					<svg class="w-3 h-3 transition-transform {scriptExpanded ? 'rotate-90' : ''}" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
					</svg>
					Script
				</button>
				{#if scriptExpanded}
					<pre class="p-2 bg-neutral-800/50 border border-neutral-800 rounded text-[10px] text-neutral-300 whitespace-pre-wrap break-all overflow-x-auto">{playbook.script}</pre>
				{/if}
			</div>
		</div>
	{:else}
		<div class="flex items-center justify-center h-full text-[10px] text-neutral-600">
			Select a playbook to view
		</div>
	{/if}
</div>
