<script lang="ts">
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { PlaybookSummary, PlaybookDetail, ExecResult } from '../types/terminal.types';
	import { SctlRestClient } from '../utils/rest-client';
	import PlaybookList from '../components/PlaybookList.svelte';
	import PlaybookViewer from '../components/PlaybookViewer.svelte';
	import PlaybookExecutor from '../components/PlaybookExecutor.svelte';

	interface Props {
		config: DeviceConnectionConfig;
		editable?: boolean;
		class?: string;
	}

	let { config, editable = false, class: className = '' }: Props = $props();

	let restClient: SctlRestClient | null = $state(null);
	let playbooks: PlaybookSummary[] = $state([]);
	let selectedPlaybook: PlaybookDetail | null = $state(null);
	let loading = $state(true);
	let error: string | null = $state(null);
	let view: 'list' | 'detail' | 'execute' = $state('list');

	$effect(() => {
		restClient = new SctlRestClient(config.wsUrl, config.apiKey);
		if (config.autoConnect !== false) {
			fetchPlaybooks();
		}
	});

	async function fetchPlaybooks() {
		if (!restClient) return;
		loading = playbooks.length === 0;
		error = null;
		try {
			playbooks = await restClient.listPlaybooks();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch playbooks';
		} finally {
			loading = false;
		}
	}

	async function selectPlaybook(name: string) {
		if (!restClient) return;
		try {
			selectedPlaybook = await restClient.getPlaybook(name);
			view = 'detail';
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to load playbook';
		}
	}

	async function deletePlaybook(name: string) {
		if (!restClient) return;
		try {
			await restClient.deletePlaybook(name);
			playbooks = playbooks.filter(p => p.name !== name);
			if (selectedPlaybook?.name === name) {
				selectedPlaybook = null;
				view = 'list';
			}
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to delete playbook';
		}
	}

	function handleExecute(pb: PlaybookDetail) {
		selectedPlaybook = pb;
		view = 'execute';
	}

	function handleResult(_result: ExecResult) {
		// Result is shown inline by PlaybookExecutor
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

<div class="playbook-widget flex flex-col h-full {className}">
	{#if loading}
		<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500 animate-pulse font-mono">
			Loading playbooks...
		</div>
	{:else if error && view === 'list'}
		<div class="flex flex-col items-center justify-center py-8 gap-2">
			<div class="text-[10px] text-red-400 font-mono">{error}</div>
			<button
				class="px-2 py-1 rounded text-[10px] text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 transition-colors font-mono"
				onclick={fetchPlaybooks}
			>Retry</button>
		</div>
	{:else if view === 'list'}
		<PlaybookList
			{playbooks}
			onselect={selectPlaybook}
			ondelete={editable ? deletePlaybook : undefined}
			onrefresh={fetchPlaybooks}
		/>
	{:else if view === 'detail'}
		<div class="flex flex-col h-full">
			<div class="flex items-center px-2 py-1 border-b border-neutral-800 shrink-0">
				<button
					class="text-[10px] text-neutral-500 hover:text-neutral-300 transition-colors font-mono"
					onclick={goBack}
				>&larr; Back</button>
			</div>
			<div class="flex-1 min-h-0">
				<PlaybookViewer
					playbook={selectedPlaybook}
					onexecute={handleExecute}
					onclose={goBack}
				/>
			</div>
		</div>
	{:else if view === 'execute'}
		<div class="flex flex-col h-full">
			<div class="flex items-center px-2 py-1 border-b border-neutral-800 shrink-0">
				<button
					class="text-[10px] text-neutral-500 hover:text-neutral-300 transition-colors font-mono"
					onclick={goBack}
				>&larr; Back</button>
			</div>
			<div class="flex-1 min-h-0">
				<PlaybookExecutor
					playbook={selectedPlaybook}
					{restClient}
					onclose={goBack}
					onresult={handleResult}
				/>
			</div>
		</div>
	{:else}
		<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500 font-mono">
			Unknown view state
		</div>
	{/if}
</div>
