<script lang="ts">
	import type { ExecResult } from '../types/terminal.types';

	interface Props {
		visible: boolean;
		serverName?: string;
		onexec?: (command: string) => Promise<ExecResult>;
		onclose?: () => void;
	}

	let { visible, serverName = '', onexec = undefined, onclose = undefined }: Props = $props();

	let command = $state('');
	let result: ExecResult | null = $state(null);
	let error: string | null = $state(null);
	let loading = $state(false);
	let history: string[] = $state([]);
	let historyIndex = $state(-1);
	let inputEl: HTMLInputElement | undefined = $state();

	const MAX_HISTORY = 20;

	$effect(() => {
		if (visible) {
			// Reset state on open
			result = null;
			error = null;
			command = '';
			historyIndex = -1;
			setTimeout(() => inputEl?.focus(), 50);
		}
	});

	async function execute() {
		const cmd = command.trim();
		if (!cmd) return;
		if (!onexec) {
			error = 'No server connected';
			return;
		}

		// Add to history
		history = [cmd, ...history.filter((h) => h !== cmd)].slice(0, MAX_HISTORY);
		historyIndex = -1;

		loading = true;
		error = null;
		result = null;
		try {
			result = await onexec(cmd);
		} catch (err) {
			error = err instanceof Error ? err.message : 'Execution failed';
		} finally {
			loading = false;
		}
	}

	function handleKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			onclose?.();
		} else if (e.key === 'Enter') {
			e.preventDefault();
			execute();
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			if (history.length > 0 && historyIndex < history.length - 1) {
				historyIndex++;
				command = history[historyIndex];
			}
		} else if (e.key === 'ArrowDown') {
			e.preventDefault();
			if (historyIndex > 0) {
				historyIndex--;
				command = history[historyIndex];
			} else if (historyIndex === 0) {
				historyIndex = -1;
				command = '';
			}
		}
	}

	function handleBackdropClick(e: MouseEvent) {
		if (e.target === e.currentTarget) onclose?.();
	}
</script>

{#if visible}
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<div class="fixed inset-0 z-40 bg-black/30" onclick={handleBackdropClick}>
		<div class="fixed top-[20%] left-1/2 -translate-x-1/2 z-50 w-[500px] max-w-[90vw] bg-neutral-900 border border-neutral-700 rounded-lg shadow-2xl">
			<!-- Input -->
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800" onkeydown={handleKeydown}>
				<span class="text-neutral-600 text-[11px] font-mono shrink-0">$</span>
				<input
					bind:this={inputEl}
					bind:value={command}
					placeholder={serverName ? `Run command on ${serverName}...` : 'Run command...'}
					class="flex-1 bg-transparent text-sm font-mono text-neutral-200 placeholder:text-neutral-600 focus:outline-none"
					disabled={loading}
				/>
				{#if loading}
					<span class="text-[10px] text-neutral-500 animate-pulse">running...</span>
				{/if}
			</div>

			<!-- Result -->
			{#if result || error}
				<div class="max-h-72 overflow-y-auto p-3 scrollbar-thin">
					{#if error}
						<div class="text-[11px] font-mono text-red-400">{error}</div>
					{:else if result}
						{#if result.stdout}
							<pre class="text-[11px] font-mono text-neutral-300 whitespace-pre-wrap break-all">{result.stdout}</pre>
						{/if}
						{#if result.stderr}
							<pre class="text-[11px] font-mono text-red-400 whitespace-pre-wrap break-all">{result.stderr}</pre>
						{/if}
						<div class="flex items-center gap-2 mt-2 pt-1 border-t border-neutral-800">
							<span class="text-[10px] font-mono
								{result.exit_code === 0 ? 'text-green-500' : 'text-red-500'}">
								exit {result.exit_code}
							</span>
							<span class="text-[10px] font-mono text-neutral-600">{result.duration_ms}ms</span>
						</div>
					{/if}
				</div>
			{/if}
		</div>
	</div>
{/if}
