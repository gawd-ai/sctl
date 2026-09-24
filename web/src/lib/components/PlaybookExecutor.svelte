<script lang="ts">
	import type { PlaybookDetail, PlaybookRunState, ExecResult, ViewerTab, WsSessionOutputMsg } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';
	import type { SctlWsClient } from '../utils/ws-client';
	import { renderPlaybookScript } from '../utils/playbook-parser';
	import { uuid } from '../utils/index';
	import { onDestroy } from 'svelte';

	interface Props {
		playbook: PlaybookDetail | null;
		restClient: SctlRestClient | null;
		/**
		 * WebSocket client. When present, the playbook runs as a streaming **job**
		 * (live output, no 30s ceiling). When absent, falls back to the legacy
		 * blocking `restClient.exec()`.
		 */
		wsClient?: SctlWsClient | null;
		onresult?: (result: ExecResult) => void;
		onRunInTerminal?: (script: string) => void;
		onOpenViewer?: (tab: ViewerTab) => void;
		onclose?: () => void;
		/**
		 * Show the built-in actions row (Terminal / Cancel / Execute with its
		 * click-twice confirm). A host that drives the run from its own
		 * controls passes `false` and calls `run()` / `cancel()` on the
		 * component instance, following `onstatechange`.
		 */
		actions?: boolean;
		/** Show the playbook description above the parameters. */
		description?: boolean;
		/** Fires whenever the run state changes, for host-rendered controls. */
		onstatechange?: (state: PlaybookRunState) => void;
	}


	let {
		playbook,
		restClient,
		wsClient = null,
		onresult,
		onRunInTerminal,
		onOpenViewer,
		onclose,
		actions = true,
		description = true,
		onstatechange
	}: Props = $props();

	// Parameter values
	let paramValues: Record<string, string> = $state({});
	let executing = $state(false);
	let scriptPreviewExpanded = $state(false);
	let confirmingExecute = $state(false);
	let result: ExecResult | null = $state(null);
	let error: string | null = $state(null);

	// Streaming job state
	let liveOutput = $state('');
	let jobSessionId: string | null = $state(null);
	let canceling = $state(false);
	let outputEl: HTMLPreElement | null = $state(null);

	// Active job subscriptions (component-scoped so we can tear them down on
	// destroy / re-run). Not reactive.
	let activeUnsubs: Array<() => void> = [];
	function teardownJobSubs() {
		for (const u of activeUnsubs) u();
		activeUnsubs = [];
	}
	onDestroy(teardownJobSubs);

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
			liveOutput = '';
		}
	});

	$effect(() => {
		onstatechange?.({
			executing,
			canCancel: executing && jobSessionId !== null,
			canceling,
			exitCode: result ? result.exit_code : null
		});
	});

	/** Run now, with no confirm step: the host's control is the confirmation. */
	export function run(): Promise<void> {
		confirmingExecute = false;
		return execute();
	}

	/** Stop the running streaming job, if any. */
	export function cancel(): Promise<void> {
		return cancelJob();
	}

	// Auto-scroll the live output to the bottom as frames arrive.
	$effect(() => {
		liveOutput;
		if (outputEl) outputEl.scrollTop = outputEl.scrollHeight;
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
		if (!playbook) return;
		const script = renderPlaybookScript(playbook.script, paramValues, playbook.params);

		// ── Streaming path (preferred): run as a job, stream output live ──
		if (wsClient) {
			teardownJobSubs();
			executing = true;
			error = null;
			result = null;
			liveOutput = '';
			canceling = false;

			let stdoutBuf = '';
			let stderrBuf = '';
			const startTime = Date.now();
			let finished = false;

			const finish = (exitCode: number) => {
				if (finished) return;
				finished = true;
				teardownJobSubs();
				const res: ExecResult = {
					exit_code: exitCode,
					stdout: stdoutBuf,
					stderr: stderrBuf,
					duration_ms: Date.now() - startTime
				};
				result = res;
				executing = false;
				jobSessionId = null;
				canceling = false;
				onresult?.(res);
			};

			try {
				const started = await wsClient.startJob({ command: script, name: `pb:${playbook.name}` });
				jobSessionId = started.session_id;

				activeUnsubs.push(
					wsClient.onOutput(started.session_id, (msg: WsSessionOutputMsg) => {
						liveOutput += msg.data;
						if (msg.type === 'session.stdout') stdoutBuf += msg.data;
						else if (msg.type === 'session.stderr') stderrBuf += msg.data;
					})
				);
				activeUnsubs.push(
					wsClient.onSessionEnd(started.session_id, (msg) => {
						finish('exit_code' in msg ? msg.exit_code : -1);
					})
				);
			} catch (e) {
				teardownJobSubs();
				error = e instanceof Error ? e.message : 'Failed to start job';
				executing = false;
				jobSessionId = null;
			}
			return;
		}

		// ── Legacy fallback (no WS client): blocking one-shot exec ──
		if (!restClient) return;
		executing = true;
		error = null;
		result = null;
		liveOutput = '';
		try {
			const execResult = await restClient.exec(script);
			result = execResult;
			liveOutput = `${execResult.stdout ?? ''}${execResult.stderr ?? ''}`;
			onresult?.(execResult);
		} catch (e) {
			error = e instanceof Error ? e.message : 'Execution failed';
		} finally {
			executing = false;
		}
	}

	async function cancelJob() {
		if (!wsClient || !jobSessionId || canceling) return;
		canceling = true;
		try {
			await wsClient.killSession(jobSessionId);
		} catch {
			// Session may have already exited — the end subscription will finalize.
		}
	}

	function openFullOutput() {
		if (!result || !playbook || !onOpenViewer) return;
		const cmd = playbook.name;
		const tab: ViewerTab = {
			key: uuid(),
			type: 'exec',
			label: cmd.length > 24 ? cmd.slice(0, 24) + '...' : cmd,
			icon: '$',
			data: {
				activityId: 0,
				command: cmd,
				exitCode: result.exit_code,
				stdout: result.stdout,
				stderr: result.stderr,
				durationMs: result.duration_ms,
				status: result.exit_code === 0 ? 'success' : 'failed'
			}
		};
		onOpenViewer(tab);
	}

	let paramEntries = $derived(
		playbook ? Object.entries(playbook.params).sort(([a], [b]) => a.localeCompare(b)) : []
	);
</script>

<!--
	Themed through --sctl-* custom properties, the same contract as
	PlaybookViewer (see its style block); defaults are the dark console.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="playbook-executor sctl-pb flex flex-col h-full">
	{#if playbook}
		<div class="flex-1 overflow-y-auto min-h-0 px-3 py-2 space-y-3">
			{#if description && playbook.description}
				<div class="pb-muted">{playbook.description}</div>
			{/if}

			<!-- Parameters form -->
			{#if paramEntries.length > 0}
				<div>
					<div class="pb-label mb-1">Parameters</div>
					<div class="space-y-1.5">
						{#each paramEntries as [name, param]}
							<div class="pb-param px-2 py-1.5">
								<div class="flex items-baseline gap-1.5 mb-1">
									<label class="pb-strong pb-code font-semibold" for="pb-param-{name}">{name}</label>
									<span class="pb-faint pb-small">{param.type}</span>
								</div>
								{#if param.description}
									<div class="pb-muted pb-small mb-1.5">{param.description}</div>
								{/if}
								{#if param.enum && param.enum.length > 0}
									<select
										id="pb-param-{name}"
										class="pb-field w-full"
										value={paramValues[name] ?? ''}
										onchange={(e) => { paramValues = { ...paramValues, [name]: (e.target as HTMLSelectElement).value }; }}
									>
										{#each param.enum as val}
											<option value={String(val)}>{String(val)}</option>
										{/each}
									</select>
								{:else}
									<input
										id="pb-param-{name}"
										type="text"
										class="pb-field w-full"
										value={paramValues[name] ?? ''}
										placeholder={param.default !== undefined ? String(param.default) : param.type}
										oninput={(e) => { paramValues = { ...paramValues, [name]: (e.target as HTMLInputElement).value }; }}
									/>
								{/if}
							</div>
						{/each}
					</div>
				</div>
			{/if}

			{#if actions}
				<div class="flex items-center gap-2">
					{#if onRunInTerminal}
						<button
							class="pb-btn pb-btn-quiet flex items-center gap-1"
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
					<div class="flex-1"></div>
					{#if executing && jobSessionId}
						<button
							class="pb-btn pb-btn-danger disabled:opacity-50 disabled:cursor-wait"
							disabled={canceling}
							onclick={cancelJob}
						>{canceling ? 'Stopping...' : 'Cancel'}</button>
					{/if}
					<button
						class="pb-btn {executing ? 'pb-btn-quiet cursor-wait' : confirmingExecute ? 'pb-btn-danger' : 'pb-btn-ok'}"
						disabled={executing}
						onclick={() => {
							if (confirmingExecute) {
								confirmingExecute = false;
								execute();
							} else {
								confirmingExecute = true;
							}
						}}
						onmouseleave={() => { confirmingExecute = false; }}
					>{executing ? 'Running...' : confirmingExecute ? 'Confirm?' : 'Execute'}</button>
				</div>
			{/if}

			<!-- Script preview (collapsible) -->
			<div>
				<button
					class="pb-label pb-toggle flex items-center gap-1 mb-1"
					onclick={() => { scriptPreviewExpanded = !scriptPreviewExpanded; }}
				>
					<svg class="w-3 h-3 transition-transform {scriptPreviewExpanded ? 'rotate-90' : ''}" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
					</svg>
					Script Preview
				</button>
				{#if scriptPreviewExpanded}
					<pre class="pb-pre whitespace-pre-wrap break-all">{previewScript}</pre>
				{/if}
			</div>

			<!-- Error -->
			{#if error}
				<div>
					<div class="pb-label pb-danger-text mb-1">Error</div>
					<div class="pb-error">{error}</div>
				</div>
			{/if}

			<!-- Live output + result -->
			{#if executing || liveOutput || result}
				<div>
					<div class="flex items-center gap-2 mb-1">
						<span class="pb-label">Output</span>
						{#if executing}
							<span class="pb-ok-text pb-small flex items-center gap-1">
								<span class="pb-pulse w-1.5 h-1.5 rounded-full animate-pulse"></span>
								running
							</span>
						{/if}
						{#if result}
							<span class="pb-small tabular-nums {result.exit_code === 0 ? 'pb-ok-text' : 'pb-danger-text'}">
								exit {result.exit_code}
							</span>
							<span class="pb-faint pb-small tabular-nums">{result.duration_ms}ms</span>
							{#if onOpenViewer}
								<div class="flex-1"></div>
								<button class="pb-btn pb-btn-info pb-small" onclick={openFullOutput}>view full output</button>
							{/if}
						{/if}
					</div>
					{#if liveOutput}
						<pre
							bind:this={outputEl}
							class="pb-pre pb-output whitespace-pre-wrap break-all overflow-y-auto">{liveOutput}</pre>
					{:else if executing}
						<div class="pb-param pb-faint p-2">Waiting for output…</div>
					{/if}
				</div>
			{/if}
		</div>
	{:else}
		<div class="pb-faint flex items-center justify-center h-full">
			No playbook selected
		</div>
	{/if}
</div>

<style>
	/* Same theme contract as PlaybookViewer. */
	.sctl-pb {
		background: var(--sctl-bg, #171717);
		color: var(--sctl-text, #d4d4d4);
		font-family: var(--sctl-font, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace);
		font-size: var(--sctl-text-xs, 10px);
	}
	.pb-strong { color: var(--sctl-text-strong, #e5e5e5); }
	.pb-muted { color: var(--sctl-text-muted, #737373); }
	.pb-faint { color: var(--sctl-text-faint, #525252); }
	.pb-small { font-size: var(--sctl-text-2xs, 9px); }
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
	.pb-param {
		background: var(--sctl-surface, rgb(38 38 38 / 0.3));
		border: 1px solid var(--sctl-border, rgb(38 38 38 / 0.5));
		border-radius: var(--sctl-radius, 0.25rem);
	}
	.pb-field {
		padding: var(--sctl-field-padding, 0.25rem 0.375rem);
		background: var(--sctl-field-bg, #262626);
		border: 1px solid var(--sctl-field-border, #404040);
		border-radius: var(--sctl-radius, 0.25rem);
		color: var(--sctl-text-strong, #e5e5e5);
		font-family: var(--sctl-font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace);
		font-size: var(--sctl-text-xs, 10px);
	}
	.pb-field:focus { outline: none; border-color: var(--sctl-focus, #737373); box-shadow: none; }
	.pb-pre {
		padding: 0.5rem;
		background: var(--sctl-code-bg, var(--sctl-surface, rgb(38 38 38 / 0.5)));
		border: 1px solid var(--sctl-border, #262626);
		border-radius: var(--sctl-radius, 0.25rem);
		color: var(--sctl-code-text, var(--sctl-text, #d4d4d4));
		font-family: var(--sctl-font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace);
		font-size: var(--sctl-text-xs, 10px);
	}
	/* Run output: its own pair, so a host can keep output terminal-dark
	   while the script preview follows the page. */
	.pb-output {
		max-height: var(--sctl-output-max-height, 16rem);
		background: var(--sctl-output-bg, var(--sctl-code-bg, var(--sctl-surface, rgb(38 38 38 / 0.5))));
		color: var(--sctl-output-text, var(--sctl-code-text, var(--sctl-text, #d4d4d4)));
	}
	.pb-error {
		padding: 0.5rem;
		background: var(--sctl-danger-bg, rgb(127 29 29 / 0.2));
		border: 1px solid var(--sctl-danger-border, rgb(127 29 29 / 0.4));
		border-radius: var(--sctl-radius, 0.25rem);
		color: var(--sctl-danger-text, #fca5a5);
	}
	.pb-ok-text { color: var(--sctl-success, #4ade80); }
	.pb-danger-text { color: var(--sctl-danger, #f87171); }
	.pb-pulse { background: var(--sctl-success, #4ade80); }
	.pb-btn {
		padding: 0.25rem 0.5rem;
		border-radius: var(--sctl-radius, 0.25rem);
		font-size: var(--sctl-text-xs, 10px);
		transition: background-color 150ms, color 150ms;
	}
	.pb-btn-ok { background: var(--sctl-success-bg, rgb(20 83 45 / 0.4)); color: var(--sctl-success, #4ade80); }
	.pb-btn-ok:hover { background: var(--sctl-success-bg-hover, rgb(20 83 45 / 0.6)); }
	.pb-btn-danger { background: var(--sctl-danger-btn-bg, rgb(127 29 29 / 0.4)); color: var(--sctl-danger, #f87171); }
	.pb-btn-danger:hover { background: var(--sctl-danger-btn-bg-hover, rgb(127 29 29 / 0.6)); }
	.pb-btn-quiet { background: var(--sctl-field-bg, #262626); color: var(--sctl-text-secondary, #a3a3a3); }
	.pb-btn-quiet:hover { color: var(--sctl-text-strong, #e5e5e5); background: var(--sctl-field-border, #404040); }
	.pb-btn-info { background: var(--sctl-accent-bg, rgb(59 130 246 / 0.15)); color: var(--sctl-accent, #60a5fa); }
	.pb-btn-info:hover { background: var(--sctl-accent-bg-hover, rgb(59 130 246 / 0.25)); }
</style>
