<script lang="ts">
	import type { DeviceInfo } from '../types/terminal.types';

	interface Props {
		info: DeviceInfo;
		onrefresh?: () => void;
	}

	let { info, onrefresh = undefined }: Props = $props();

	function formatUptime(secs: number): string {
		const d = Math.floor(secs / 86400);
		const h = Math.floor((secs % 86400) / 3600);
		const m = Math.floor((secs % 3600) / 60);
		if (d > 0) return `${d}d ${h}h ${m}m`;
		if (h > 0) return `${h}h ${m}m`;
		return `${m}m`;
	}

	function formatBytes(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(0)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	function pct(used: number, total: number): number {
		return total > 0 ? (used / total) * 100 : 0;
	}

	function barColor(percent: number): string {
		if (percent >= 90) return 'bg-red-500';
		if (percent >= 70) return 'bg-amber-500';
		return 'bg-green-500';
	}

	let memPct = $derived(pct(info.memory.used_bytes, info.memory.total_bytes));
	let diskPct = $derived(pct(info.disk.used_bytes, info.disk.total_bytes));

	let upInterfaces = $derived(
		info.interfaces.filter((i) => i.state === 'up' || i.state === 'UP')
	);
</script>

<div class="px-1 py-1.5 text-[10px] font-mono text-neutral-400 space-y-1">
	<!-- Header with refresh -->
	<div class="flex items-center gap-1">
		<span class="text-neutral-300 truncate flex-1">{info.hostname}</span>
		<span class="text-neutral-600 truncate">{info.kernel}</span>
		<button
			class="w-4 h-4 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors shrink-0"
			title="Refresh"
			onclick={onrefresh}
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
			</svg>
		</button>
	</div>

	<!-- Uptime -->
	<div class="flex items-center gap-1">
		<span class="text-neutral-600">up</span>
		<span>{formatUptime(info.system_uptime_secs)}</span>
	</div>

	<!-- CPU + load -->
	<div class="flex items-center gap-1">
		<span class="text-neutral-600">cpu</span>
		<span class="truncate">{info.cpu_model}</span>
	</div>
	<div class="flex items-center gap-1">
		<span class="text-neutral-600">load</span>
		<span class="tabular-nums">{info.load_average.map((l) => l.toFixed(2)).join(' ')}</span>
	</div>

	<!-- Memory bar -->
	<div class="flex items-center gap-1">
		<span class="text-neutral-600 w-6">mem</span>
		<div class="flex-1 h-1.5 bg-neutral-800 rounded-full overflow-hidden">
			<div class="h-full rounded-full transition-all {barColor(memPct)}" style:width="{memPct}%"></div>
		</div>
		<span class="tabular-nums text-neutral-500">{formatBytes(info.memory.used_bytes)}/{formatBytes(info.memory.total_bytes)}</span>
	</div>

	<!-- Disk bar -->
	<div class="flex items-center gap-1">
		<span class="text-neutral-600 w-6">disk</span>
		<div class="flex-1 h-1.5 bg-neutral-800 rounded-full overflow-hidden">
			<div class="h-full rounded-full transition-all {barColor(diskPct)}" style:width="{diskPct}%"></div>
		</div>
		<span class="tabular-nums text-neutral-500">{formatBytes(info.disk.used_bytes)}/{formatBytes(info.disk.total_bytes)}</span>
	</div>

	<!-- Network interfaces -->
	{#if upInterfaces.length > 0}
		<div class="space-y-0.5">
			{#each upInterfaces as iface}
				<div class="flex items-center gap-1">
					<span class="text-neutral-600">{iface.name}</span>
					<span class="truncate">{iface.addresses.filter((a) => !a.includes(':')).join(', ') || iface.mac}</span>
				</div>
			{/each}
		</div>
	{/if}

	<!-- Tunnel -->
	{#if info.tunnel}
		<div class="flex items-center gap-1">
			<span class="text-neutral-600">tunnel</span>
			<span class="w-1.5 h-1.5 rounded-full {info.tunnel.connected ? 'bg-green-500' : 'bg-neutral-600'}"></span>
			<span class="truncate">{info.tunnel.url}</span>
		</div>
	{/if}
</div>
