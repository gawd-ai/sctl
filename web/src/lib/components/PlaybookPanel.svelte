<script lang="ts">
	import type { SctlRestClient } from '../utils/rest-client';
	import type { PlaybookSummary, PlaybookDetail, ViewerTab } from '../types/terminal.types';
	import PlaybookList from './PlaybookList.svelte';
	import PlaybookViewer from './PlaybookViewer.svelte';
	import PlaybookExecutor from './PlaybookExecutor.svelte';

	interface Props {
		visible?: boolean;
		restClient: SctlRestClient | null;
		onRunInTerminal?: (script: string) => void;
		onOpenViewer?: (tab: ViewerTab) => void;
	}

	let {
		visible = true,
		restClient,
		onRunInTerminal,
		onOpenViewer
	}: Props = $props();

	// ── State machine: list → detail → execute ───────────────────
	type View = 'list' | 'detail' | 'execute';
	let view: View = $state('list');
	let playbooks: PlaybookSummary[] = $state([]);
	let selectedPlaybook = $state<PlaybookDetail | null>(null);
	let loading = $state(false);
	let error: string | null = $state(null);

	// ── Data fetching ────────────────────────────────────────────

	let lastRestClient: typeof restClient = null;

	$effect(() => {
		if (visible && restClient) {
			if (restClient !== lastRestClient) {
				lastRestClient = restClient;
				view = 'list';
				selectedPlaybook = null;
				fetchPlaybooks();
			}
		}
	});

	async function fetchPlaybooks() {
		if (!restClient) return;
		loading = true;
		error = null;
		try {
			playbooks = await restClient.listPlaybooks();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch playbooks';
			playbooks = [];
		} finally {
			loading = false;
		}
	}

	// Pending name shown in header while detail loads
	let pendingName: string | null = $state(null);
	let headerName = $derived(selectedPlaybook?.name ?? pendingName);

	async function selectPlaybook(name: string) {
		if (!restClient) return;
		pendingName = name;
		view = 'detail';
		loading = true;
		error = null;
		try {
			selectedPlaybook = await restClient.getPlaybook(name);
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch playbook';
		} finally {
			loading = false;
			pendingName = null;
		}
	}

	async function deletePlaybook(name: string) {
		if (!restClient) return;
		try {
			await restClient.deletePlaybook(name);
			if (selectedPlaybook?.name === name) {
				selectedPlaybook = null;
				view = 'list';
			}
			await fetchPlaybooks();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to delete playbook';
		}
	}

	function goBack() {
		if (view === 'execute') {
			view = 'detail';
		} else {
			view = 'list';
			selectedPlaybook = null;
		}
	}
</script>

<div class="flex-1 min-w-0 bg-neutral-900 flex flex-col h-full">
			<!-- Back header (shown in detail/execute views) -->
			{#if view !== 'list'}
				<div class="flex items-center gap-1 px-1.5 h-8 border-b border-neutral-700 shrink-0">
					<button
						class="w-5 h-5 shrink-0 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Back"
						onclick={goBack}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M15 19l-7-7 7-7" />
						</svg>
					</button>
					{#if headerName}
						<span class="flex-1 min-w-0 text-[11px] text-neutral-200 font-mono font-semibold truncate">{headerName}</span>
						{#if view === 'detail'}
							<button
								class="shrink-0 px-2 py-0.5 rounded text-[10px] transition-colors
									{loading ? 'bg-green-900/20 text-green-800 cursor-not-allowed' : 'bg-green-900/40 text-green-400 hover:bg-green-900/60'}"
								disabled={loading}
								onclick={() => { view = 'execute'; }}
							>Run</button>
						{/if}
					{/if}
				</div>
			{/if}

			<!-- Error -->
			{#if error}
				<div class="px-3 py-2 text-[10px] text-red-400">{error}</div>
			{/if}

			<!-- Views -->
			<div class="flex-1 min-h-0 overflow-hidden relative">
				{#if view === 'list'}
					<PlaybookList
						{playbooks}
						onselect={selectPlaybook}
						ondelete={deletePlaybook}
						onrefresh={fetchPlaybooks}
					/>
				{:else if view === 'detail' && selectedPlaybook}
					<PlaybookViewer
						playbook={selectedPlaybook}
					/>
				{:else if view === 'execute' && selectedPlaybook}
					<PlaybookExecutor
						playbook={selectedPlaybook}
						{restClient}
						{onRunInTerminal}
						{onOpenViewer}
					/>
				{/if}
				{#if loading}
					<div class="absolute inset-0 bg-neutral-900/60 flex items-center justify-center">
						<span class="text-[10px] text-neutral-500">Loading...</span>
					</div>
				{/if}
			</div>
</div>
