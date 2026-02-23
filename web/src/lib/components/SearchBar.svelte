<script lang="ts">
	interface Props {
		matchCount?: number;
		currentMatch?: number;
		onclose?: () => void;
		onsearch?: (term: string, opts: { caseSensitive: boolean; regex: boolean }) => void;
		onnext?: () => void;
		onprev?: () => void;
	}

	let {
		matchCount = undefined,
		currentMatch = undefined,
		onclose = undefined,
		onsearch = undefined,
		onnext = undefined,
		onprev = undefined
	}: Props = $props();

	let searchTerm = $state('');
	let caseSensitive = $state(false);
	let useRegex = $state(false);
	let inputEl: HTMLInputElement | undefined = $state();
	let debounceTimer: ReturnType<typeof setTimeout> | undefined;

	$effect(() => {
		inputEl?.focus();
	});

	$effect(() => {
		return () => {
			clearTimeout(debounceTimer);
		};
	});

	function handleInput() {
		clearTimeout(debounceTimer);
		debounceTimer = setTimeout(() => {
			onsearch?.(searchTerm, { caseSensitive, regex: useRegex });
		}, 150);
	}

	function handleKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			onclose?.();
		} else if (e.key === 'Enter') {
			e.preventDefault();
			if (e.shiftKey) onprev?.();
			else onnext?.();
		}
	}

	function toggleCase() {
		caseSensitive = !caseSensitive;
		onsearch?.(searchTerm, { caseSensitive, regex: useRegex });
	}

	function toggleRegex() {
		useRegex = !useRegex;
		onsearch?.(searchTerm, { caseSensitive, regex: useRegex });
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
	class="absolute top-2 right-2 z-10 flex items-center gap-1 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded shadow-lg"
	onkeydown={handleKeydown}
>
	<input
		bind:this={inputEl}
		bind:value={searchTerm}
		oninput={handleInput}
		placeholder="Search..."
		class="w-48 px-1.5 py-0.5 bg-neutral-900 border border-neutral-700 rounded text-[11px] font-mono text-neutral-200 focus:outline-none focus:border-neutral-500"
	/>
	<!-- Prev -->
	<button
		class="w-5 h-5 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-700 transition-colors"
		title="Previous (Shift+Enter)"
		onclick={onprev}
	>
		<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
			<path stroke-linecap="round" stroke-linejoin="round" d="M5 15l7-7 7 7" />
		</svg>
	</button>
	<!-- Next -->
	<button
		class="w-5 h-5 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-700 transition-colors"
		title="Next (Enter)"
		onclick={onnext}
	>
		<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
			<path stroke-linecap="round" stroke-linejoin="round" d="M19 9l-7 7-7-7" />
		</svg>
	</button>
	<!-- Case sensitive -->
	<button
		class="w-5 h-5 flex items-center justify-center rounded text-[10px] font-bold transition-colors
			{caseSensitive ? 'text-neutral-200 bg-neutral-700' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-700'}"
		title="Match case"
		onclick={toggleCase}
	>Aa</button>
	<!-- Regex -->
	<button
		class="w-5 h-5 flex items-center justify-center rounded text-[10px] font-mono transition-colors
			{useRegex ? 'text-neutral-200 bg-neutral-700' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-700'}"
		title="Use regex"
		onclick={toggleRegex}
	>.*</button>
	<!-- Match count -->
	{#if matchCount !== undefined}
		<span class="text-[10px] text-neutral-500 tabular-nums min-w-8 text-center">
			{matchCount > 0 ? `${(currentMatch ?? 0) + 1}/${matchCount}` : 'No results'}
		</span>
	{/if}
	<!-- Close -->
	<button
		class="w-5 h-5 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-700 transition-colors"
		title="Close (Esc)"
		onclick={onclose}
	>
		<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
			<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
		</svg>
	</button>
</div>
