<script lang="ts">
	import { untrack } from 'svelte';
	import type { DirEntry } from '../types/terminal.types';

	interface Props {
		entries: DirEntry[];
		currentPath: string;
		selectedName: string | null;
		selectedNames: Set<string>;
		focusedIndex: number;
		filterText: string;
		showHidden: boolean;
		renamingName: string | null;
		creatingType: 'file' | 'dir' | null;
		loading: boolean;
		error: string | null;
		readonly: boolean;
		confirmingDelete?: boolean;
		onselect?: (entry: DirEntry, e?: MouseEvent) => void;
		onopen?: (entry: DirEntry) => void;
		onnavigate?: (path: string) => void;
		oncontextmenu?: (e: MouseEvent, entry: DirEntry | null) => void;
		onrenamesubmit?: (oldName: string, newName: string) => void;
		onrenamecancel?: () => void;
		oncreatesubmit?: (name: string) => void;
		oncreatecancel?: () => void;
		onretry?: () => void;
		onfocuschange?: (index: number) => void;
	}

	let {
		entries,
		currentPath,
		selectedName,
		selectedNames,
		focusedIndex,
		filterText,
		showHidden,
		renamingName,
		creatingType,
		loading,
		error,
		readonly,
		confirmingDelete = false,
		onselect,
		onopen,
		onnavigate,
		oncontextmenu,
		onrenamesubmit,
		onrenamecancel,
		oncreatesubmit,
		oncreatecancel,
		onretry,
		onfocuschange
	}: Props = $props();

	let renameInput: HTMLInputElement | undefined = $state();
	let createInput: HTMLInputElement | undefined = $state();
	let treeContainer: HTMLDivElement | undefined = $state();

	// Local input values — managed here to avoid parent round-trip on every keystroke
	let renameValue = $state('');
	let createValue = $state('');

	const isDir = (e: DirEntry) => e.type === 'dir';

	// Sort: dirs first, then alphabetical
	let sortedEntries = $derived(
		[...entries]
			.filter((e) => {
				if (!showHidden && e.name.startsWith('.')) return false;
				if (filterText) return e.name.toLowerCase().includes(filterText.toLowerCase());
				return true;
			})
			.sort((a, b) => {
				if (isDir(a) && !isDir(b)) return -1;
				if (!isDir(a) && isDir(b)) return 1;
				return a.name.localeCompare(b.name);
			})
	);

	// Auto-focus rename/create inputs
	// Initialize rename value + focus + select (runs once when entering rename mode)
	$effect(() => {
		if (renamingName && renameInput) {
			untrack(() => {
				renameValue = renamingName;
				renameInput!.focus();
				const dotIdx = renamingName.lastIndexOf('.');
				renameInput!.setSelectionRange(0, dotIdx > 0 ? dotIdx : renamingName.length);
			});
		}
	});

	// Reset create value when entering create mode
	$effect(() => {
		if (creatingType) {
			createValue = '';
		}
	});

	$effect(() => {
		if (creatingType && createInput) {
			createInput.focus();
		}
	});

	// Scroll focused item into view
	$effect(() => {
		if (focusedIndex >= 0 && treeContainer) {
			const items = treeContainer.querySelectorAll('[data-tree-item]');
			items[focusedIndex]?.scrollIntoView({ block: 'nearest' });
		}
	});

	function formatSize(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	function iconColor(entry: DirEntry): string {
		if (isDir(entry)) return 'text-amber-400';
		if (entry.type === 'symlink') return 'text-purple-400';
		const ext = entry.name.split('.').pop()?.toLowerCase() ?? '';
		switch (ext) {
			case 'ts': case 'tsx': return 'text-blue-400';
			case 'js': case 'jsx': return 'text-yellow-400';
			case 'rs': return 'text-orange-400';
			case 'json': case 'toml': case 'yaml': case 'yml': return 'text-green-400';
			case 'sh': case 'bash': case 'zsh': return 'text-emerald-400';
			case 'md': case 'txt': return 'text-neutral-300';
			case 'svelte': return 'text-orange-500';
			case 'css': case 'scss': return 'text-pink-400';
			case 'html': return 'text-red-400';
			case 'py': return 'text-sky-400';
			case 'go': return 'text-cyan-400';
			case 'lock': return 'text-neutral-600';
			default: return 'text-neutral-500';
		}
	}

	function handleRenameKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') {
			e.preventDefault();
			if (renameValue.trim() && renamingName) onrenamesubmit?.(renamingName, renameValue.trim());
		} else if (e.key === 'Escape') {
			e.preventDefault();
			onrenamecancel?.();
		}
	}

	function handleCreateKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') {
			e.preventDefault();
			if (createValue.trim()) oncreatesubmit?.(createValue.trim());
		} else if (e.key === 'Escape') {
			e.preventDefault();
			oncreatecancel?.();
		}
	}

	function handleEntryClick(entry: DirEntry, e: MouseEvent) {
		if (renamingName === entry.name) return;
		onselect?.(entry, e);
	}

	function handleEntryDblClick(entry: DirEntry) {
		onopen?.(entry);
	}

	function navigateUp() {
		const parent = currentPath.replace(/\/[^/]+\/?$/, '') || '/';
		onnavigate?.(parent);
	}

	function highlightMatch(name: string): string {
		if (!filterText) return name;
		const idx = name.toLowerCase().indexOf(filterText.toLowerCase());
		if (idx === -1) return name;
		const before = name.slice(0, idx);
		const match = name.slice(idx, idx + filterText.length);
		const after = name.slice(idx + filterText.length);
		return `${before}<mark class="bg-yellow-500/30 text-yellow-200 rounded-sm">${match}</mark>${after}`;
	}
</script>

<div
	bind:this={treeContainer}
	class="flex-1 overflow-y-auto min-h-0"
	oncontextmenu={(e) => { e.preventDefault(); oncontextmenu?.(e, null); }}
>
	{#if loading}
		<div class="flex flex-col gap-1 px-2 py-2">
			{#each Array(8) as _}
				<div class="flex items-center gap-2 h-6">
					<div class="w-3.5 h-3.5 bg-neutral-800 rounded animate-pulse"></div>
					<div class="h-3 bg-neutral-800 rounded animate-pulse" style="width: {40 + Math.random() * 120}px"></div>
				</div>
			{/each}
		</div>
	{:else if error}
		<div class="flex flex-col items-center justify-center py-8 gap-2">
			<span class="text-[10px] text-red-400">{error}</span>
			{#if onretry}
				<button
					class="px-2 py-0.5 rounded text-[10px] text-neutral-400 bg-neutral-800 hover:bg-neutral-700 hover:text-neutral-200 transition-colors"
					onclick={onretry}
				>retry</button>
			{/if}
		</div>
	{:else}
		<!-- Parent directory -->
		{#if currentPath !== '/'}
			<button
				class="w-full flex items-center gap-1.5 px-2 py-0.5 text-[11px] font-mono text-neutral-500 hover:bg-neutral-800/60 transition-colors h-6"
				data-tree-item
				onclick={navigateUp}
			>
				<svg class="w-3.5 h-3.5 text-neutral-600 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M15 18l-6-6 6-6" />
				</svg>
				<span>..</span>
			</button>
		{/if}

		<!-- Create new entry input (at top) -->
		{#if creatingType}
			<div class="flex items-center gap-1.5 px-2 py-0.5 bg-neutral-800/60 h-6">
				<svg class="w-3.5 h-3.5 shrink-0 {creatingType === 'dir' ? 'text-amber-400' : 'text-blue-400'}" viewBox="0 0 20 20" fill="currentColor">
					{#if creatingType === 'dir'}
						<path d="M2 6a2 2 0 012-2h5l2 2h5a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z" />
					{:else}
						<path fill-rule="evenodd" d="M4 4a2 2 0 012-2h4.586A2 2 0 0112 2.586L15.414 6A2 2 0 0116 7.414V16a2 2 0 01-2 2H6a2 2 0 01-2-2V4z" clip-rule="evenodd" />
					{/if}
				</svg>
				<input
					bind:this={createInput}
					type="text"
					bind:value={createValue}
					onkeydown={handleCreateKeydown}
					onblur={() => oncreatecancel?.()}
					placeholder={creatingType === 'dir' ? 'folder name' : 'file name'}
					class="flex-1 min-w-0 px-1 py-0 bg-neutral-900 border border-neutral-600 rounded text-[11px] text-neutral-200 font-mono
						placeholder:text-neutral-600 focus:outline-none focus:border-neutral-400 h-4"
				/>
			</div>
		{/if}

		<!-- File entries -->
		{#each sortedEntries as entry, idx (entry.name)}
			{@const isFocused = idx === focusedIndex}
			{@const isSelected = selectedNames.has(entry.name)}
			{@const isRenaming = entry.name === renamingName}
			{@const isDeleteTarget = confirmingDelete && isSelected}
			<button
				class="w-full flex items-center gap-1.5 px-2 py-0.5 text-[11px] font-mono transition-colors text-left h-6
					{isDeleteTarget ? 'bg-red-900/30 text-red-400' : ''}
					{isFocused && !isDeleteTarget ? 'bg-neutral-800/80' : ''}
					{isSelected && !isFocused && !isDeleteTarget ? 'bg-neutral-800/50' : ''}
					{!isSelected && !isFocused ? 'hover:bg-neutral-800/40' : ''}
					{isSelected && !isDeleteTarget ? 'text-neutral-200' : ''}
					{!isSelected && !isDeleteTarget ? 'text-neutral-400' : ''}"
				data-tree-item
				onclick={(e) => handleEntryClick(entry, e)}
				ondblclick={() => handleEntryDblClick(entry)}
				oncontextmenu={(e) => { e.preventDefault(); e.stopPropagation(); onselect?.(entry); oncontextmenu?.(e, entry); }}
			>
				<!-- Icon -->
				<span class="w-3.5 h-3.5 flex items-center justify-center shrink-0 {iconColor(entry)}">
					{#if isDir(entry)}
						<svg class="w-3.5 h-3.5" viewBox="0 0 20 20" fill="currentColor">
							<path d="M2 6a2 2 0 012-2h5l2 2h5a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z" />
						</svg>
					{:else if entry.type === 'symlink'}
						<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M13.828 10.172a4 4 0 00-5.656 0l-4 4a4 4 0 105.656 5.656l1.102-1.101" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M10.172 13.828a4 4 0 005.656 0l4-4a4 4 0 00-5.656-5.656l-1.102 1.101" />
						</svg>
					{:else}
						<svg class="w-3.5 h-3.5" viewBox="0 0 20 20" fill="currentColor">
							<path fill-rule="evenodd" d="M4 4a2 2 0 012-2h4.586A2 2 0 0112 2.586L15.414 6A2 2 0 0116 7.414V16a2 2 0 01-2 2H6a2 2 0 01-2-2V4z" clip-rule="evenodd" />
						</svg>
					{/if}
				</span>

				<!-- Name (or rename input) -->
				{#if isRenaming}
					<!-- svelte-ignore a11y_no_static_element_interactions -->
					<div class="flex-1 min-w-0" onclick={(e) => e.stopPropagation()}>
						<input
							bind:this={renameInput}
							type="text"
							bind:value={renameValue}
							onkeydown={handleRenameKeydown}
							onblur={() => onrenamecancel?.()}
							class="w-full px-1 py-0 bg-neutral-900 border border-neutral-600 rounded text-[11px] text-neutral-200 font-mono
								focus:outline-none focus:border-neutral-400 h-4"
						/>
					</div>
				{:else}
					<span class="flex-1 truncate">
						{#if filterText}
							{@html highlightMatch(entry.name)}
						{:else}
							{entry.name}
						{/if}
						{#if entry.type === 'symlink' && entry.symlink_target}
							<span class="text-neutral-600"> &rarr; {entry.symlink_target}</span>
						{/if}
					</span>
				{/if}

				<!-- Mode + Size -->
				{#if !isRenaming}
					{#if entry.mode}
						<span class="text-neutral-700 text-[9px] tabular-nums shrink-0 font-mono">{entry.mode}</span>
					{/if}
					{#if !isDir(entry)}
						<span class="text-neutral-600 text-[9px] tabular-nums shrink-0">{formatSize(entry.size)}</span>
					{/if}
				{/if}
			</button>
		{/each}

		{#if sortedEntries.length === 0 && !loading}
			<div class="flex items-center justify-center py-8 text-[10px] text-neutral-600">
				{filterText ? 'No matches' : 'Empty directory'}
			</div>
		{/if}
	{/if}
</div>
