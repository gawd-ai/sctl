<script lang="ts">
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { DeviceInfo } from '../types/terminal.types';
	import { SctlRestClient } from '../utils/rest-client';
	import DeviceInfoPanel from '../components/DeviceInfoPanel.svelte';

	/** Displays device info (hostname, CPU, memory, disk, network) with periodic polling. */
	interface Props {
		/** Connection details (wsUrl, apiKey). Required. */
		config: DeviceConnectionConfig;
		/** Polling interval in ms. Default: 30000. Set 0 to disable polling. */
		pollInterval?: number;
		/** Additional CSS classes on the wrapper div. */
		class?: string;
	}

	let { config, pollInterval = 30000, class: className = '' }: Props = $props();

	let info: DeviceInfo | null = $state(null);
	let loading = $state(true);
	let error: string | null = $state(null);
	let client: SctlRestClient | null = $state(null);
	let intervalId: ReturnType<typeof setInterval> | null = null;

	$effect(() => {
		if (intervalId) {
			clearInterval(intervalId);
			intervalId = null;
		}

		client = new SctlRestClient(config.wsUrl, config.apiKey);
		if (config.autoConnect !== false) {
			fetchInfo();
		}

		if (pollInterval > 0) {
			intervalId = setInterval(fetchInfo, pollInterval);
		}

		return () => {
			if (intervalId) clearInterval(intervalId);
		};
	});

	async function fetchInfo() {
		if (!client) return;
		loading = info === null;
		error = null;
		try {
			info = await client.getInfo();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to fetch device info';
		} finally {
			loading = false;
		}
	}
</script>

<div class="device-status-widget {className}">
	{#if loading}
		<div class="flex items-center justify-center py-8 text-[10px] text-neutral-500 animate-pulse font-mono">
			Connecting...
		</div>
	{:else if error}
		<div class="flex flex-col items-center justify-center py-8 gap-2">
			<div class="text-[10px] text-red-400 font-mono">{error}</div>
			<button
				class="px-2 py-1 rounded text-[10px] text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 transition-colors font-mono"
				onclick={fetchInfo}
			>Retry</button>
		</div>
	{:else if info}
		<DeviceInfoPanel {info} onrefresh={fetchInfo} />
	{/if}
</div>
