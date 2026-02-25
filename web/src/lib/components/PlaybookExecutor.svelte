<script lang="ts">
	import type { PlaybookDetail, ExecResult } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';
	import { renderPlaybookScript } from '../utils/playbook-parser';

	interface Props {
		playbook: PlaybookDetail | null;
		restClient: SctlRestClient | null;
		onclose?: () => void;
		onresult?: (result: ExecResult) => void;
		onRunInTerminal?: (script: string) => void;
	}

	let { playbook, restClient, onclose, onresult, onRunInTerminal }: Props = $props();

	// Parameter values
	let paramValues: Record<string, string> = $state({});
	let executing = $state(false);
	let result: ExecResult | null = $state(null);
	let error: string | null = $state(null);

	// Initialize param values from defaults when playbook changes
	$effect(() => {
		if (playbook) {
			const values: Record<string, string> = {};
			for (const [name, param] of Object.entries(playbook.params)) {
				values[name] = param.default !== undefined ? String(param.default) : '';
			}
			paramValues = values;
			result = null;
			error = null;
		}
	});

	// Live script preview
	let previewScript = $derived((() => {
		if (!playbook) return '';
		try {
			return renderPlaybookScript(playbook.script, paramValues, playbook.params);
		} catch {
			return playbook.script;
		}
	})());

	async function execute() {
		if (!playbook || !restClient) return;

		executing = true;
		error = null;
		result = null;

		try {
			const script = renderPlaybookScript(playbook.script, paramValues, playbook.params);
			const execResult = await restClient.exec(script);
			result = execResult;
			onresult?.(execResult);
		} catch (e) {
			error = e instanceof Error ? e.message : 'Execution failed';
		} finally {
			executing = false;
		}
	}

	let paramEntries = $derived(
		playbook ? Object.entries(playbook.params).sort(([a], [b]) => a.localeCompare(b)) : []
	);
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-executor flex flex-col h-full bg-neutral-900 font-mono">
	{#if playbook}
		<!-- Header -->
		<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800 shrink-0">
			<div class="flex-1 min-w-0">
				<div class="text-xs text-neutral-200 font-semibold truncate">{playbook.name}</div>
				<div class="text-[10px] text-neutral-500">Execute playbook</div>
			</div>
			{#if onRunInTerminal}
				<button
					class="px-2 py-1 rounded text-[10px] transition-colors bg-green-900/40 text-green-400 hover:bg-green-900/60 flex items-center gap-1"
					onclick={() => onRunInTerminal?.(previewScript)}
					title="Send script to active terminal session"
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<polyline points="4 17 10 11 4 5" />
						<line x1="12" y1="19" x2="20" y2="19" />
					</svg>
					Terminal
				</button>
			{/if}
			<button
				class="px-2 py-1 rounded text-[10px] transition-colors
					{executing
						? 'bg-neutral-800 text-neutral-500 cursor-wait'
						: 'bg-neutral-800 text-neutral-400 hover:text-neutral-200 hover:bg-neutral-700'}"
				disabled={executing}
				onclick={execute}
			>{executing ? 'Running...' : 'Execute'}</button>
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
			<!-- Parameters form -->
			{#if paramEntries.length > 0}
				<div>
					<div class="text-[10px] text-neutral-500 uppercase tracking-wide mb-1">Parameters</div>
					<div class="space-y-1.5">
						{#each paramEntries as [name, param]}
							<div>
								<label class="flex items-center gap-2 text-[10px]">
									<span class="text-neutral-400 w-24 shrink-0 truncate" title={param.description}>{name}</span>
									{#if param.enum && param.enum.length > 0}
										<select
											class="flex-1 px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 focus:outline-none focus:border-neutral-500"
											value={paramValues[name] ?? ''}
											onchange={(e) => { paramValues = { ...paramValues, [name]: (e.target as HTMLSelectElement).value }; }}
										>
											{#each param.enum as val}
												<option value={String(val)}>{String(val)}</option>
											{/each}
										</select>
									{:else}
										<input
											type="text"
											class="flex-1 px-1.5 py-1 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 focus:outline-none focus:border-neutral-500"
											value={paramValues[name] ?? ''}
											placeholder={param.default !== undefined ? String(param.default) : `${param.type}`}
											oninput={(e) => { paramValues = { ...paramValues, [name]: (e.target as HTMLInputElement).value }; }}
										/>
									{/if}
								</label>
								{#if param.description}
									<div class="text-[9px] text-neutral-600 ml-26 pl-[104px]">{param.description}</div>
								{/if}
							</div>
						{/each}
					</div>
				</div>
			{/if}

			<!-- Script preview -->
			<div>
				<div class="text-[10px] text-neutral-500 uppercase tracking-wide mb-1">Script Preview</div>
				<pre class="p-2 bg-neutral-800/50 border border-neutral-800 rounded text-[10px] text-neutral-300 whitespace-pre-wrap break-all">{previewScript}</pre>
			</div>

			<!-- Result -->
			{#if error}
				<div>
					<div class="text-[10px] text-red-400 uppercase tracking-wide mb-1">Error</div>
					<div class="p-2 bg-red-900/20 border border-red-900/40 rounded text-[10px] text-red-300">{error}</div>
				</div>
			{/if}

			{#if result}
				<div>
					<div class="flex items-center gap-2 mb-1">
						<span class="text-[10px] text-neutral-500 uppercase tracking-wide">Result</span>
						<span class="text-[9px] tabular-nums {result.exit_code === 0 ? 'text-green-400' : 'text-red-400'}">
							exit {result.exit_code}
						</span>
						<span class="text-[9px] text-neutral-600 tabular-nums">{result.duration_ms}ms</span>
					</div>
					{#if result.stdout}
						<div class="mb-1">
							<div class="text-[9px] text-neutral-600 mb-0.5">stdout</div>
							<pre class="p-2 bg-neutral-800/50 border border-neutral-800 rounded text-[10px] text-neutral-300 whitespace-pre-wrap break-all max-h-48 overflow-y-auto">{result.stdout}</pre>
						</div>
					{/if}
					{#if result.stderr}
						<div>
							<div class="text-[9px] text-neutral-600 mb-0.5">stderr</div>
							<pre class="p-2 bg-red-900/10 border border-red-900/30 rounded text-[10px] text-red-300/80 whitespace-pre-wrap break-all max-h-48 overflow-y-auto">{result.stderr}</pre>
						</div>
					{/if}
				</div>
			{/if}
		</div>
	{:else}
		<div class="flex items-center justify-center h-full text-[10px] text-neutral-600">
			No playbook selected
		</div>
	{/if}
</div>
