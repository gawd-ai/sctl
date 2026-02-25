<script lang="ts">
	import type { SctlRestClient } from '../utils/rest-client';
	import type { PlaybookSummary, PlaybookDetail } from '../types/terminal.types';
	import PlaybookList from './PlaybookList.svelte';
	import PlaybookViewer from './PlaybookViewer.svelte';
	import PlaybookExecutor from './PlaybookExecutor.svelte';

	interface Props {
		visible?: boolean;
		restClient: SctlRestClient | null;
		onRunInTerminal?: (script: string) => void;
	}

	let {
		visible = true,
		restClient,
		onRunInTerminal
	}: Props = $props();

	// ── State machine: list → detail → execute ───────────────────
	type View = 'list' | 'detail' | 'execute';
	let view: View = $state('list');
	let playbooks: PlaybookSummary[] = $state([]);
	let selectedPlaybook: PlaybookDetail | null = $state(null);
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

	async function selectPlaybook(name: string) {
		if (!restClient) return;
		loading = true;
		error = null;
		try {
			selectedPlaybook = await restClient.getPlaybook(name);
			view = 'detail';
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch playbook';
		} finally {
			loading = false;
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
						class="w-5 h-5 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Back"
						onclick={goBack}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M15 19l-7-7 7-7" />
						</svg>
					</button>
				</div>
			{/if}

			<!-- Loading / error -->
			{#if loading}
				<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500">Loading...</div>
			{:else if error}
				<div class="px-3 py-2 text-[10px] text-red-400">{error}</div>
			{/if}

			<!-- Views -->
			<div class="flex-1 min-h-0 overflow-hidden">
				{#if view === 'list' && !loading}
					<PlaybookList
						{playbooks}
						onselect={selectPlaybook}
						ondelete={deletePlaybook}
						onrefresh={fetchPlaybooks}
					/>
				{:else if view === 'detail' && selectedPlaybook && !loading}
					<PlaybookViewer
						playbook={selectedPlaybook}
						onexecute={(pb) => { view = 'execute'; }}
						onclose={goBack}
					/>
				{:else if view === 'execute' && selectedPlaybook && !loading}
					<PlaybookExecutor
						playbook={selectedPlaybook}
						{restClient}
						{onRunInTerminal}
						onclose={goBack}
					/>
				{/if}
			</div>
</div>
