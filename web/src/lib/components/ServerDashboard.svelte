<script lang="ts">
	import type { ConnectionStatus, DeviceInfo, ActivityEntry, ViewerTab } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';
	import HistoryViewer from './HistoryViewer.svelte';

	interface Props {
		visible: boolean;
		connectionStatus: ConnectionStatus;
		deviceInfo: DeviceInfo | null;
		activity: ActivityEntry[];
		restClient: SctlRestClient | null;
		onrefreshinfo?: () => void;
		onToggleFiles?: () => void;
		onTogglePlaybooks?: () => void;
		onToggleAi?: () => void;
		onOpenViewer?: (tab: ViewerTab) => void;
		sidePanelOpen?: boolean;
		sidePanelTab?: string;
		masterAiEnabled?: boolean;
		rightInset?: number;
		rightInsetAnimate?: boolean;
	}

	let {
		visible,
		connectionStatus,
		deviceInfo,
		activity,
		restClient,
		onrefreshinfo,
		onToggleFiles,
		onTogglePlaybooks,
		onToggleAi,
		onOpenViewer,
		sidePanelOpen = false,
		sidePanelTab = '',
		masterAiEnabled = false,
		rightInset = 0,
		rightInsetAnimate = false
	}: Props = $props();

	// ── Auto-refresh ────────────────────────────────────────────────

	let refreshCountdown = $state(30);

	$effect(() => {
		if (!visible || connectionStatus !== 'connected') return;
		refreshCountdown = 30;
		const interval = setInterval(() => {
			refreshCountdown--;
			if (refreshCountdown <= 0) {
				onrefreshinfo?.();
				refreshCountdown = 30;
			}
		}, 1000);
		return () => clearInterval(interval);
	});

	// ── Helpers ──────────────────────────────────────────────────────

	function formatUptime(secs: number): string {
		const d = Math.floor(secs / 86400);
		const h = Math.floor((secs % 86400) / 3600);
		const m = Math.floor((secs % 3600) / 60);
		const parts: string[] = [];
		if (d > 0) parts.push(`${d}d`);
		if (h > 0) parts.push(`${h}h`);
		parts.push(`${m}m`);
		return parts.join(' ');
	}

	function formatBytes(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)}K`;
		if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)}M`;
		return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)}G`;
	}

	function pct(used: number, total: number): number {
		if (total <= 0) return 0;
		return Math.min(Math.round((used / total) * 100), 100);
	}

	function loadColor(load: number): string {
		if (load < 1.0) return 'bg-green-500/80';
		if (load < 2.0) return 'bg-amber-500/80';
		return 'bg-red-500/80';
	}

	function usageColor(percent: number): string {
		if (percent < 70) return 'bg-green-500/70';
		if (percent < 90) return 'bg-amber-500/70';
		return 'bg-red-500/70';
	}

	function usageTextColor(percent: number): string {
		if (percent < 70) return 'text-green-400/80';
		if (percent < 90) return 'text-amber-400/80';
		return 'text-red-400/80';
	}

	function dotColor(status: ConnectionStatus): string {
		switch (status) {
			case 'connected': return 'bg-green-500';
			case 'connecting': case 'reconnecting': return 'bg-yellow-500 animate-pulse';
			default: return 'bg-neutral-600';
		}
	}

	function statusLabel(status: ConnectionStatus): string {
		switch (status) {
			case 'connected': return 'connected';
			case 'connecting': return 'connecting';
			case 'reconnecting': return 'reconnecting';
			default: return 'offline';
		}
	}
</script>

<div class="flex flex-col h-full bg-neutral-950">
	<!-- Main two-column area -->
	<div class="flex flex-1 min-h-0 font-mono"
		 style:margin-right="{rightInset}px"
		 style:transition={rightInsetAnimate ? 'margin 300ms ease-in-out' : 'none'}>
		<!-- Left column: System Monitor -->
		<div class="w-1/2 border-r border-neutral-800/60 overflow-y-auto p-3 space-y-3">
			{#if deviceInfo}
				<!-- System header -->
				<div class="space-y-0.5">
					<div class="flex items-baseline gap-2">
						<span class="text-sm text-neutral-200">{deviceInfo.hostname}</span>
						<span class="text-[10px] text-neutral-600">{deviceInfo.kernel}</span>
					</div>
					<div class="flex items-center gap-2 text-[10px]">
						<span class="text-neutral-500">up</span>
						<span class="text-green-400/70">{formatUptime(deviceInfo.system_uptime_secs)}</span>
						<span class="text-neutral-700">|</span>
						<span class="text-neutral-600">{deviceInfo.serial}</span>
					</div>
				</div>

				<!-- CPU -->
				<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
					<div class="flex items-baseline justify-between">
						<span class="text-[10px] text-neutral-600">[ cpu ]</span>
					</div>
					<div class="text-[10px] text-neutral-500 truncate">{deviceInfo.cpu_model}</div>
					<div class="space-y-1">
						{#each [
							{ label: '1m', val: deviceInfo.load_average[0] },
							{ label: '5m', val: deviceInfo.load_average[1] },
							{ label: '15m', val: deviceInfo.load_average[2] }
						] as { label, val }}
							{@const maxLoad = Math.max(4, ...deviceInfo.load_average)}
							<div class="flex items-center gap-2">
								<span class="w-6 text-right text-[9px] text-neutral-600">{label}</span>
								<div class="flex-1 h-1.5 bg-neutral-800 rounded-full overflow-hidden">
									<div
										class="h-full rounded-full transition-all {loadColor(val)}"
										style="width: {Math.min(val / maxLoad * 100, 100)}%"
									></div>
								</div>
								<span class="w-8 text-right text-[9px] text-neutral-400 tabular-nums">{val.toFixed(2)}</span>
							</div>
						{/each}
					</div>
				</div>

				<!-- Memory -->
				{@const memPct = pct(deviceInfo.memory.used_bytes, deviceInfo.memory.total_bytes)}
				<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
					<div class="flex items-baseline justify-between">
						<span class="text-[10px] text-neutral-600">[ memory ]</span>
						<span class="text-[10px] tabular-nums {usageTextColor(memPct)}">{memPct}%</span>
					</div>
					<div class="h-2 bg-neutral-800 rounded-full overflow-hidden">
						<div
							class="h-full rounded-full transition-all {usageColor(memPct)}"
							style="width: {memPct}%"
						></div>
					</div>
					<div class="text-[10px] text-neutral-500">
						{formatBytes(deviceInfo.memory.used_bytes)} / {formatBytes(deviceInfo.memory.total_bytes)}
						<span class="text-neutral-700">|</span>
						<span class="text-neutral-600">{formatBytes(deviceInfo.memory.available_bytes)} free</span>
					</div>
				</div>

				<!-- Disk -->
				{@const diskPct = pct(deviceInfo.disk.used_bytes, deviceInfo.disk.total_bytes)}
				<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
					<div class="flex items-baseline justify-between">
						<span class="text-[10px] text-neutral-600">[ disk <span class="text-neutral-700">{deviceInfo.disk.mount_point}</span> ]</span>
						<span class="text-[10px] tabular-nums {usageTextColor(diskPct)}">{diskPct}%</span>
					</div>
					<div class="h-2 bg-neutral-800 rounded-full overflow-hidden">
						<div
							class="h-full rounded-full transition-all {usageColor(diskPct)}"
							style="width: {diskPct}%"
						></div>
					</div>
					<div class="text-[10px] text-neutral-500">
						{formatBytes(deviceInfo.disk.used_bytes)} / {formatBytes(deviceInfo.disk.total_bytes)}
						<span class="text-neutral-700">|</span>
						<span class="text-neutral-600">{formatBytes(deviceInfo.disk.available_bytes)} free</span>
					</div>
				</div>

				<!-- Network -->
				{@const upInterfaces = deviceInfo.interfaces.filter(i => i.state === 'up' || i.state === 'UP' || i.state === 'unknown')}
				{#if upInterfaces.length > 0}
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
						<span class="text-[10px] text-neutral-600">[ network ]</span>
						{#each upInterfaces as iface}
							<div class="flex items-center gap-2 text-[10px]">
								<span class="text-neutral-500 w-14 truncate">{iface.name}</span>
								{#if iface.addresses.length > 0}
									<span class="text-neutral-300">{iface.addresses.join(', ')}</span>
								{:else}
									<span class="text-neutral-700">{iface.mac}</span>
								{/if}
							</div>
						{/each}
					</div>
				{/if}

				<!-- Tunnel -->
				{#if deviceInfo.tunnel}
					<div class="border border-neutral-800/60 rounded p-2.5">
						<div class="flex items-center gap-2 text-[10px]">
							<span class="text-neutral-600">[ tunnel ]</span>
							<span class="w-1.5 h-1.5 rounded-full shrink-0 {deviceInfo.tunnel.connected ? 'bg-green-500' : 'bg-neutral-600'}"></span>
							<span class="text-neutral-400 truncate">{deviceInfo.tunnel.url}</span>
						</div>
					</div>
				{/if}

				{:else if connectionStatus === 'connected'}
				<div class="flex items-center justify-center h-32 text-[10px] text-neutral-600">
					loading system info...
				</div>
			{:else}
				<div class="flex items-center justify-center h-32 text-[10px] text-neutral-700">
					not connected
				</div>
			{/if}
		</div>

		<!-- Right column: Activity History -->
		<div class="w-1/2 min-h-0 overflow-hidden">
			<HistoryViewer entries={activity} {restClient} {onOpenViewer} />
		</div>
	</div>

	<!-- Status bar (layout matches ControlBar: left content, spacer, right buttons) -->
	<div class="flex items-center px-1.5 py-1 bg-neutral-900 border-t border-neutral-800 text-[10px] text-neutral-500 h-7">
		<!-- Connection status + refresh (left) -->
		<span class="w-1.5 h-1.5 rounded-full shrink-0 {dotColor(connectionStatus)}"></span>
		<span class="text-neutral-600 ml-1.5">{statusLabel(connectionStatus)}</span>

		<span class="text-neutral-700 mx-1.5">&middot;</span>

		{#if connectionStatus === 'connected'}
			<button
				class="tabular-nums text-neutral-600 hover:text-neutral-300 transition-colors"
				onclick={() => { onrefreshinfo?.(); refreshCountdown = 30; }}
				title="Refresh now"
			>&#8635; {String(refreshCountdown).padStart(2, '0')}s</button>
		{:else}
			<span class="tabular-nums text-neutral-700">paused</span>
		{/if}

		<!-- Spacer -->
		<div class="flex-1"></div>

		<!-- Panel & AI controls (right, matches ControlBar) -->
		<div class="flex items-center gap-0.5">
			{#if onToggleFiles}
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors
						{sidePanelOpen && sidePanelTab === 'files' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
					onclick={onToggleFiles}
					title="Toggle file browser (Alt+E)"
				>
					<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
					</svg>
				</button>
			{/if}
			{#if onTogglePlaybooks}
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors
						{sidePanelOpen && sidePanelTab === 'playbooks' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
					onclick={onTogglePlaybooks}
					title="Toggle playbooks (Alt+B)"
				>
					<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M12 6.253v13m0-13C10.832 5.477 9.246 5 7.5 5S4.168 5.477 3 6.253v13C4.168 18.477 5.754 18 7.5 18s3.332.477 4.5 1.253m0-13C13.168 5.477 14.754 5 16.5 5c1.747 0 3.332.477 4.5 1.253v13C19.832 18.477 18.247 18 16.5 18c-1.746 0-3.332.477-4.5 1.253" />
					</svg>
				</button>
			{/if}
			<button
				class="w-5 h-5 flex items-center justify-center rounded transition-colors
					{masterAiEnabled
						? 'bg-amber-900/50 text-amber-400 hover:bg-amber-900/70'
						: 'text-neutral-600 hover:bg-neutral-800 hover:text-neutral-400'}"
				onclick={onToggleAi}
				title={masterAiEnabled ? 'AI enabled for all sessions — click to disable' : 'AI disabled — click to enable for all sessions'}
			>
				{#if masterAiEnabled}
					<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="currentColor">
						<path d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
					</svg>
				{:else}
					<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
						<path stroke-linecap="round" stroke-linejoin="round" d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
					</svg>
				{/if}
			</button>
		</div>
	</div>
</div>
