<script lang="ts">
	import type { SctlRestClient } from '../utils/rest-client';
	import type { DirEntry } from '../types/terminal.types';

	interface Props {
		visible: boolean;
		restClient: SctlRestClient | null;
		onclose?: () => void;
	}

	let { visible, restClient, onclose = undefined }: Props = $props();

	let currentPath = $state('/');
	let entries: DirEntry[] = $state([]);
	let loading = $state(false);
	let error: string | null = $state(null);

	// File preview
	let previewPath: string | null = $state(null);
	let previewContent: string | null = $state(null);
	let previewLoading = $state(false);
	let previewError: string | null = $state(null);

	let lastLoadedPath: string | null = null;
	let lastRestClient: typeof restClient = null;

	$effect(() => {
		if (visible && restClient) {
			// Only reload if the restClient changed (server switch) or first open
			if (restClient !== lastRestClient) {
				lastRestClient = restClient;
				lastLoadedPath = null;
				currentPath = '/';
			}
			if (lastLoadedPath !== currentPath) {
				loadDir(currentPath);
			}
		} else if (!visible) {
			lastRestClient = null;
			lastLoadedPath = null;
		}
	});

	async function loadDir(path: string) {
		if (!restClient) return;
		loading = true;
		error = null;
		previewPath = null;
		previewContent = null;
		try {
			entries = await restClient.listDir(path);
			currentPath = path;
			lastLoadedPath = path;
		} catch (err) {
			error = err instanceof Error ? err.message : 'Failed to list directory';
			entries = [];
		} finally {
			loading = false;
		}
	}

	async function handleEntryClick(entry: DirEntry) {
		const fullPath = currentPath === '/' ? `/${entry.name}` : `${currentPath}/${entry.name}`;
		if (entry.type === 'directory') {
			await loadDir(fullPath);
		} else {
			await previewFile(fullPath, entry);
		}
	}

	async function previewFile(path: string, entry: DirEntry) {
		if (!restClient) return;
		// Skip large files (>1MB)
		if (entry.size > 1048576) {
			previewPath = path;
			previewContent = null;
			previewError = `File too large (${formatSize(entry.size)})`;
			return;
		}
		previewLoading = true;
		previewError = null;
		previewPath = path;
		previewContent = null;
		try {
			const result = await restClient.readFile(path);
			previewContent = result.content;
		} catch (err) {
			previewError = err instanceof Error ? err.message : 'Failed to read file';
		} finally {
			previewLoading = false;
		}
	}

	function navigateUp() {
		const parent = currentPath.replace(/\/[^/]+\/?$/, '') || '/';
		loadDir(parent);
	}

	function navigateTo(idx: number) {
		const parts = currentPath.split('/').filter(Boolean);
		const path = '/' + parts.slice(0, idx + 1).join('/');
		loadDir(path);
	}

	function formatSize(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	function fileIcon(entry: DirEntry): string {
		if (entry.type === 'directory') return '📁';
		if (entry.type === 'symlink') return '🔗';
		return '📄';
	}

	let sortedEntries = $derived(
		[...entries].sort((a, b) => {
			// Directories first
			if (a.type === 'directory' && b.type !== 'directory') return -1;
			if (a.type !== 'directory' && b.type === 'directory') return 1;
			return a.name.localeCompare(b.name);
		})
	);

	let breadcrumbs = $derived(currentPath.split('/').filter(Boolean));
</script>

{#if visible}
	<div class="w-96 h-full bg-neutral-900 border-l border-neutral-700 flex flex-col shrink-0">
		<!-- Header: breadcrumb + controls -->
		<div class="flex items-center gap-1 px-2 py-1.5 border-b border-neutral-800 min-h-8">
			<!-- Breadcrumb -->
			<div class="flex items-center gap-0.5 flex-1 min-w-0 overflow-x-auto scrollbar-none text-[10px] font-mono">
				<button
					class="text-neutral-400 hover:text-neutral-200 transition-colors shrink-0"
					onclick={() => loadDir('/')}
				>/</button>
				{#each breadcrumbs as crumb, i}
					<span class="text-neutral-600">/</span>
					<button
						class="text-neutral-400 hover:text-neutral-200 transition-colors truncate max-w-24"
						onclick={() => navigateTo(i)}
					>{crumb}</button>
				{/each}
			</div>
			<!-- Refresh -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
				title="Refresh"
				onclick={() => loadDir(currentPath)}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
				</svg>
			</button>
			<!-- Close -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
				title="Close"
				onclick={onclose}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
				</svg>
			</button>
		</div>

		<!-- File list -->
		<div class="flex-1 overflow-y-auto min-h-0 {previewPath ? 'max-h-[50%]' : ''}">
			{#if loading}
				<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500 animate-pulse">Loading...</div>
			{:else if error}
				<div class="flex items-center justify-center py-8 text-[10px] text-red-400">{error}</div>
			{:else}
				<!-- Parent directory -->
				{#if currentPath !== '/'}
					<button
						class="w-full flex items-center gap-2 px-2 py-1 text-[11px] font-mono text-neutral-400 hover:bg-neutral-800 transition-colors"
						onclick={navigateUp}
					>
						<span class="w-4 text-center">..</span>
						<span class="text-neutral-600">parent directory</span>
					</button>
				{/if}
				{#each sortedEntries as entry (entry.name)}
					<button
						class="w-full flex items-center gap-2 px-2 py-1 text-[11px] font-mono hover:bg-neutral-800 transition-colors text-left
							{previewPath && previewPath.endsWith('/' + entry.name) ? 'bg-neutral-800 text-neutral-200' : 'text-neutral-400'}"
						onclick={() => handleEntryClick(entry)}
					>
						<span class="w-4 text-center text-[10px]">{fileIcon(entry)}</span>
						<span class="flex-1 truncate">{entry.name}{entry.type === 'symlink' && entry.symlink_target ? ` → ${entry.symlink_target}` : ''}</span>
						{#if entry.type !== 'directory'}
							<span class="text-neutral-600 text-[9px] tabular-nums shrink-0">{formatSize(entry.size)}</span>
						{/if}
					</button>
				{/each}
				{#if sortedEntries.length === 0 && !loading}
					<div class="flex items-center justify-center py-8 text-[10px] text-neutral-600">Empty directory</div>
				{/if}
			{/if}
		</div>

		<!-- File preview -->
		{#if previewPath}
			<div class="border-t border-neutral-700 flex flex-col min-h-0 flex-1">
				<div class="flex items-center gap-1 px-2 py-1 bg-neutral-800/50">
					<span class="text-[10px] font-mono text-neutral-400 truncate flex-1">{previewPath.split('/').pop()}</span>
					<button
						class="w-4 h-4 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 transition-colors"
						title="Close preview"
						onclick={() => { previewPath = null; previewContent = null; previewError = null; }}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
						</svg>
					</button>
				</div>
				<div class="flex-1 overflow-auto p-2">
					{#if previewLoading}
						<div class="text-[10px] text-neutral-500 animate-pulse">Loading...</div>
					{:else if previewError}
						<div class="text-[10px] text-red-400">{previewError}</div>
					{:else if previewContent !== null}
						<pre class="text-[11px] font-mono text-neutral-300 whitespace-pre-wrap break-all">{previewContent}</pre>
					{/if}
				</div>
			</div>
		{/if}
	</div>
{/if}
