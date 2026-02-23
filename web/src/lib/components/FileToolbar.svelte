<script lang="ts">
	interface Props {
		currentPath: string;
		filterText: string;
		showHidden: boolean;
		cdSync: boolean;
		selectionCount: number;
		readonly: boolean;
		onnavigate?: (path: string) => void;
		onfilterchange?: (text: string) => void;
		ontogglehidden?: () => void;
		ontogglecdsync?: () => void;
		onrefresh?: () => void;
		onnewfile?: () => void;
		onnewfolder?: () => void;
	}

	let {
		currentPath,
		filterText,
		showHidden,
		cdSync,
		selectionCount,
		readonly,
		onnavigate,
		onfilterchange,
		ontogglehidden,
		ontogglecdsync,
		onrefresh,
		onnewfile,
		onnewfolder
	}: Props = $props();

	let showFilter = $state(false);
	let filterInput: HTMLInputElement | undefined = $state();

	let breadcrumbs = $derived(currentPath.split('/').filter(Boolean));

	function navigateTo(idx: number) {
		const path = '/' + breadcrumbs.slice(0, idx + 1).join('/');
		onnavigate?.(path);
	}

	function toggleFilter() {
		showFilter = !showFilter;
		if (!showFilter) {
			onfilterchange?.('');
		} else {
			// Focus input after render
			requestAnimationFrame(() => filterInput?.focus());
		}
	}

	function handleFilterKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			showFilter = false;
			onfilterchange?.('');
		}
	}
</script>

<div class="flex flex-col shrink-0">
	<!-- Breadcrumb + action buttons -->
	<div class="flex items-center gap-0.5 px-1.5 h-8 border-b border-neutral-700">
		<!-- Breadcrumb -->
		<div class="flex items-center gap-0.5 flex-1 min-w-0 overflow-x-auto scrollbar-none text-[10px] font-mono">
			<button
				class="text-neutral-400 hover:text-neutral-200 transition-colors shrink-0 px-0.5"
				onclick={() => onnavigate?.('/')}
			>/</button>
			{#each breadcrumbs as crumb, i}
				<span class="text-neutral-600 shrink-0">&gt;</span>
				<button
					class="text-neutral-400 hover:text-neutral-200 transition-colors truncate max-w-24 px-0.5"
					onclick={() => navigateTo(i)}
				>{crumb}</button>
			{/each}
		</div>

		<!-- Selection count -->
		{#if selectionCount > 1}
			<span class="text-[9px] text-neutral-500 tabular-nums shrink-0 bg-neutral-800 px-1 rounded">{selectionCount} sel</span>
		{/if}

		<!-- Action buttons -->
		{#if !readonly}
			<!-- New file -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
				title="New file"
				onclick={onnewfile}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" />
					<path stroke-linecap="round" stroke-linejoin="round" d="M14 2v6h6M12 18v-6M9 15h6" />
				</svg>
			</button>
			<!-- New folder -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
				title="New folder"
				onclick={onnewfolder}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z" />
					<path stroke-linecap="round" stroke-linejoin="round" d="M12 11v6M9 14h6" />
				</svg>
			</button>
		{/if}
		<!-- Search/filter toggle -->
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors shrink-0
				{showFilter ? 'text-neutral-200 bg-neutral-800' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			title="Filter files"
			onclick={toggleFilter}
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<circle cx="11" cy="11" r="8" />
				<path stroke-linecap="round" stroke-linejoin="round" d="M21 21l-4.35-4.35" />
			</svg>
		</button>
		<!-- Dotfiles toggle -->
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors shrink-0
				{showHidden ? 'text-neutral-200 bg-neutral-800' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			title={showHidden ? 'Hide dotfiles' : 'Show dotfiles'}
			onclick={ontogglehidden}
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				{#if showHidden}
					<path stroke-linecap="round" stroke-linejoin="round" d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
					<circle cx="12" cy="12" r="3" />
				{:else}
					<path stroke-linecap="round" stroke-linejoin="round" d="M17.94 17.94A10.07 10.07 0 0112 20c-7 0-11-8-11-8a18.45 18.45 0 015.06-5.94M9.9 4.24A9.12 9.12 0 0112 4c7 0 11 8 11 8a18.5 18.5 0 01-2.16 3.19m-6.72-1.07a3 3 0 11-4.24-4.24" />
					<line x1="1" y1="1" x2="23" y2="23" />
				{/if}
			</svg>
		</button>
		<!-- cd sync toggle -->
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors shrink-0
				{cdSync ? 'text-green-400 bg-neutral-800' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			title={cdSync ? 'cd sync ON — navigating syncs terminal' : 'cd sync OFF'}
			onclick={ontogglecdsync}
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
			</svg>
		</button>
		<!-- Refresh -->
		<button
			class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
			title="Refresh"
			onclick={onrefresh}
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
			</svg>
		</button>
	</div>

	<!-- Filter input (toggleable) -->
	{#if showFilter}
		<div class="px-1.5 py-1 border-b border-neutral-700">
			<input
				bind:this={filterInput}
				type="text"
				value={filterText}
				oninput={(e) => onfilterchange?.(e.currentTarget.value)}
				onkeydown={handleFilterKeydown}
				placeholder="filter..."
				class="w-full px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded text-[10px] text-neutral-200 font-mono
					placeholder:text-neutral-600 focus:outline-none focus:border-neutral-500"
			/>
		</div>
	{/if}
</div>
