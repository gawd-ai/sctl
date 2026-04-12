<script lang="ts">
	import { untrack } from 'svelte';
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { ActivityEntry, WsActivityNewMsg } from '../types/terminal.types';
	import { SctlRestClient } from '../utils/rest-client';
	import { SctlWsClient } from '../utils/ws-client';
	import ActivityFeed from '../components/ActivityFeed.svelte';

	/** Activity feed showing device operations with optional real-time WebSocket updates. */
	interface Props {
		/** Connection details (wsUrl, apiKey). Required. */
		config: DeviceConnectionConfig;
		/** Max entries to display. Default: 100. */
		maxEntries?: number;
		/** Subscribe to live updates via WebSocket. Default: true. */
		realtime?: boolean;
		/** Additional CSS classes on the wrapper div. */
		class?: string;
	}

	let { config, maxEntries = 100, realtime = true, class: className = '' }: Props = $props();

	let entries: ActivityEntry[] = $state([]);
	let loading = $state(true);
	let error: string | null = $state(null);

	let restClient: SctlRestClient | null = null;
	let wsClient: SctlWsClient | null = null;
	let unsubWs: (() => void) | null = null;

	let _prevWsUrl = '';
	let _prevApiKey = '';

	$effect(() => {
		const wsUrl = config.wsUrl;
		const apiKey = config.apiKey;
		const autoConnect = config.autoConnect;
		const rt = realtime;
		const max = maxEntries;

		untrack(() => {
			if (wsUrl !== _prevWsUrl || apiKey !== _prevApiKey) {
				_prevWsUrl = wsUrl;
				_prevApiKey = apiKey;

				// Cleanup previous
				if (unsubWs) { unsubWs(); unsubWs = null; }
				if (wsClient) { wsClient.disconnect(); wsClient = null; }

				restClient = new SctlRestClient(wsUrl, apiKey);
				fetchActivity();

				if (rt) {
					wsClient = new SctlWsClient(wsUrl, apiKey);
					if (autoConnect !== false) {
						wsClient.connect();
					}
					unsubWs = wsClient.on('activity.new', (msg: WsActivityNewMsg) => {
						entries = [...entries, msg.entry].slice(-max);
					});
				}
			}
		});

		return () => {
			if (unsubWs) {
				unsubWs();
				unsubWs = null;
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
