<script lang="ts">
	import type { PlaybookDetail } from '../types/terminal.types';

	interface Props {
		playbook: PlaybookDetail | null;
		onexecute?: (playbook: PlaybookDetail) => void;
		onedit?: (playbook: PlaybookDetail) => void;
		onclose?: () => void;
		/**
		 * Show the built-in header (name, description, Run/Edit/Close). A host
		 * that renders its own title and actions passes `false`.
		 */
		header?: boolean;
	}

	let { playbook, onexecute, onedit, onclose, header = true }: Props = $props();

	let paramEntries = $derived(
		playbook ? Object.entries(playbook.params).sort(([a], [b]) => a.localeCompare(b)) : []
	);

	let scriptExpanded = $state(false);
</script>

<!--
	Themed through --sctl-* custom properties (see the style block); the
	defaults are the dark console look, so a host restyles it by setting the
	variables on any ancestor.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-viewer sctl-pb flex flex-col h-full">
	{#if playbook}
		{#if header}
			<div class="pb-divider flex items-center gap-2 px-3 py-2 border-b shrink-0">
				<div class="flex-1 min-w-0">
					<div class="pb-title font-semibold truncate">{playbook.name}</div>
					<div class="pb-muted truncate">{playbook.description}</div>
				</div>
				{#if onexecute}
					<button class="pb-btn pb-btn-ok" onclick={() => onexecute?.(playbook)}>Run</button>
				{/if}
				{#if onedit}
					<button class="pb-btn pb-btn-quiet" onclick={() => onedit?.(playbook)}>Edit</button>
				{/if}
				{#if onclose}
					<button
						class="pb-icon-btn w-5 h-5 flex items-center justify-center"
						onclick={onclose}
						aria-label="Close"
					>
						<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
						</svg>
					</button>
				{/if}
			</div>
		{/if}

		<div class="flex-1 overflow-y-auto min-h-0 px-3 py-2 space-y-3">
			<!-- Parameters -->
			{#if paramEntries.length > 0}
				<div>
					<div class="pb-label mb-1">Parameters</div>
					<div class="pb-box overflow-hidden">
						<table class="pb-table w-full">
							<thead>
								<tr>
									<th>Name</th>
									<th>Type</th>
									<th>Description</th>
									<th>Default</th>
								</tr>
							</thead>
							<tbody>
								{#each paramEntries as [name, param]}
									<tr>
										<td class="pb-strong pb-code">{name}</td>
										<td class="pb-muted">{param.type}</td>
										<td class="pb-secondary">{param.description}</td>
										<td class="pb-faint pb-code">
											{param.default !== undefined ? String(param.default) : '-'}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				</div>
			{/if}

			<!-- Script (collapsible) -->
			<div>
				<button
					class="pb-label pb-toggle flex items-center gap-1 mb-1"
					onclick={() => { scriptExpanded = !scriptExpanded; }}
				>
					<svg class="w-3 h-3 transition-transform {scriptExpanded ? 'rotate-90' : ''}" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
					</svg>
					Script
				</button>
				{#if scriptExpanded}
					<pre class="pb-pre whitespace-pre-wrap break-all overflow-x-auto">{playbook.script}</pre>
				{/if}
			</div>
		</div>
	{:else}
		<div class="pb-faint flex items-center justify-center h-full">
			Select a playbook to view
		</div>
	{/if}
</div>

<style>
	/* Theme contract, shared with PlaybookExecutor: set any --sctl-* on an
	   ancestor to restyle. Defaults reproduce the dark console. */
	.sctl-pb {
		background: var(--sctl-bg, #171717);
		color: var(--sctl-text, #d4d4d4);
		font-family: var(--sctl-font, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace);
		font-size: var(--sctl-text-xs, 10px);
	}
	.pb-divider { border-color: var(--sctl-border, #262626); }
	.pb-title { color: var(--sctl-text-strong, #e5e5e5); font-size: var(--sctl-text-sm, 12px); }
	.pb-strong { color: var(--sctl-text-strong, #e5e5e5); }
	.pb-secondary { color: var(--sctl-text-secondary, #a3a3a3); }
	.pb-muted { color: var(--sctl-text-muted, #737373); }
	.pb-faint { color: var(--sctl-text-faint, #525252); }
	.pb-code { font-family: var(--sctl-font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace); }
	.pb-label {
		color: var(--sctl-text-muted, #737373);
		font-size: var(--sctl-text-label, var(--sctl-text-xs, 10px));
		font-weight: var(--sctl-label-weight, 400);
		text-transform: uppercase;
		letter-spacing: 0.025em;
	}
	.pb-toggle { transition: color 150ms; }
	.pb-toggle:hover { color: var(--sctl-text-secondary, #a3a3a3); }
	.pb-box {
		border: 1px solid var(--sctl-border, #262626);
		border-radius: var(--sctl-radius, 0.25rem);
	}
	.pb-table { font-size: var(--sctl-text-xs, 10px); }
	.pb-table th {
		text-align: left;
		padding: 0.25rem 0.5rem;
		font-weight: 400;
		color: var(--sctl-text-muted, #737373);
		background: var(--sctl-surface, rgb(38 38 38 / 0.5));
	}
	.pb-table td { padding: 0.25rem 0.5rem; border-top: 1px solid var(--sctl-border, rgb(38 38 38 / 0.5)); }
	.pb-pre {
		padding: 0.5rem;
		background: var(--sctl-code-bg, var(--sctl-surface, rgb(38 38 38 / 0.5)));
		border: 1px solid var(--sctl-border, #262626);
		border-radius: var(--sctl-radius, 0.25rem);
		color: var(--sctl-code-text, var(--sctl-text, #d4d4d4));
		font-family: var(--sctl-font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace);
		font-size: var(--sctl-text-xs, 10px);
	}
	.pb-btn {
		padding: 0.25rem 0.5rem;
		border-radius: var(--sctl-radius, 0.25rem);
		font-size: var(--sctl-text-xs, 10px);
		transition: background-color 150ms, color 150ms;
	}
	.pb-btn-ok { background: var(--sctl-success-bg, rgb(20 83 45 / 0.4)); color: var(--sctl-success, #4ade80); }
	.pb-btn-ok:hover { background: var(--sctl-success-bg-hover, rgb(20 83 45 / 0.6)); }
	.pb-btn-quiet { background: var(--sctl-field-bg, #262626); color: var(--sctl-text-secondary, #a3a3a3); }
	.pb-btn-quiet:hover { color: var(--sctl-text-strong, #e5e5e5); background: var(--sctl-field-border, #404040); }
	.pb-icon-btn { border-radius: var(--sctl-radius, 0.25rem); color: var(--sctl-text-muted, #737373); }
	.pb-icon-btn:hover { color: var(--sctl-text, #d4d4d4); background: var(--sctl-field-bg, #262626); }
</style>
