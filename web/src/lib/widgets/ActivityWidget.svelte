<script lang="ts">
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { ActivityEntry, WsActivityNewMsg } from '../types/terminal.types';
	import { SctlRestClient } from '../utils/rest-client';
	import { SctlWsClient } from '../utils/ws-client';
	import ActivityFeed from '../components/ActivityFeed.svelte';

	interface Props {
		config: DeviceConnectionConfig;
		maxEntries?: number;
		realtime?: boolean;
		class?: string;
	}

	let { config, maxEntries = 100, realtime = true, class: className = '' }: Props = $props();

	let entries: ActivityEntry[] = $state([]);
	let loading = $state(true);
	let error: string | null = $state(null);

	let restClient: SctlRestClient | null = null;
	let wsClient: SctlWsClient | null = null;
	let unsubscribe: (() => void) | null = null;

	$effect(() => {
		restClient = new SctlRestClient(config.wsUrl, config.apiKey);
		fetchActivity();

		if (realtime) {
			wsClient = new SctlWsClient(config.wsUrl, config.apiKey);
			if (config.autoConnect !== false) {
				wsClient.connect();
			}
			unsubscribe = wsClient.on('activity.new', (msg: WsActivityNewMsg) => {
				entries = [...entries, msg.entry].slice(-maxEntries);
			});
		}

		return () => {
			if (unsubscribe) {
				unsubscribe();
				unsubscribe = null;
			}
			if (wsClient) {
				wsClient.disconnect();
				wsClient = null;
			}
		};
	});

	async function fetchActivity() {
		if (!restClient) return;
		loading = entries.length === 0;
		error = null;
		try {
			entries = await restClient.getActivity(0, maxEntries);
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch activity';
		} finally {
			loading = false;
		}
	}
</script>

<div class="activity-widget {className}">
	{#if loading}
		<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500 animate-pulse font-mono">
			Loading activity...
		</div>
	{:else if error}
		<div class="flex flex-col items-center justify-center py-8 gap-2">
			<div class="text-[10px] text-red-400 font-mono">{error}</div>
			<button
				class="px-2 py-1 rounded text-[10px] text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 transition-colors font-mono"
				onclick={fetchActivity}
			>Retry</button>
		</div>
	{:else}
		<ActivityFeed {entries} />
	{/if}
</div>
