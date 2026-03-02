<script lang="ts">
	import type { ConnectionStatus, DeviceInfo, ActivityEntry, ViewerTab, RelayHealthInfo, DeviceProbeResult, ConnectionEvent, ServerDiagnostics, RelayConnectionSession } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';
	import HistoryViewer from './HistoryViewer.svelte';

	type DashboardView = 'device' | 'relay';

	interface Props {
		visible: boolean;
		connectionStatus: ConnectionStatus;
		deviceInfo: DeviceInfo | null;
		activity: ActivityEntry[];
		restClient: SctlRestClient | null;
		relayHealth: RelayHealthInfo | null;
		isRelay: boolean;
		relaySerial: string | null;
		disconnectReason: string | null;
		lastConnectedAt: number | null;
		deviceProbe: DeviceProbeResult | null;
		connectionLog: ConnectionEvent[];
		onrefreshrelayhealth?: () => void;
		onrefreshrelayinfo?: () => void;
		onprobedevice?: () => void;
		onrefreshinfo?: () => void;
		serverDiagnostics?: ServerDiagnostics | null;
		onfetchdiagnostics?: () => void;
		relayDiagnostics?: ServerDiagnostics | null;
		onfetchrelaydiagnostics?: () => void;
		relayInfo?: DeviceInfo | null;
		hasRelayApiKey?: boolean;
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
		relayHealth,
		isRelay,
		relaySerial,
		disconnectReason,
		lastConnectedAt,
		deviceProbe,
		connectionLog,
		onrefreshrelayhealth,
		onrefreshrelayinfo,
		onprobedevice,
		onrefreshinfo,
		serverDiagnostics = null,
		onfetchdiagnostics,
		relayDiagnostics = null,
		onfetchrelaydiagnostics,
		relayInfo = null,
		hasRelayApiKey = false,
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

	// ── Diagnostics ─────────────────────────────────────────────────

	let diagLoading = $state(false);
	let relayDiagLoading = $state(false);
	// Clear loading state when data arrives
	$effect(() => { if (serverDiagnostics) diagLoading = false; });
	$effect(() => { if (relayDiagnostics) relayDiagLoading = false; });

	// Auto-fetch relay diagnostics when relay view becomes visible
	$effect(() => {
		if (visible && isRelay && hasRelayApiKey && !relayDiagnostics && !relayDiagLoading) {
			relayDiagLoading = true;
			onfetchrelaydiagnostics?.();
		}
	});

	// ── View toggle ─────────────────────────────────────────────────

	let activeView: DashboardView = $state('device');
	let prevStatus: ConnectionStatus | null = $state(null);

	// Auto-switch to relay when device goes offline, back to device when it reconnects
	$effect(() => {
		if (connectionStatus === 'device_offline' && isRelay) {
			activeView = 'relay';
		} else if (connectionStatus === 'connected' && prevStatus === 'device_offline') {
			activeView = 'device';
		}
		prevStatus = connectionStatus;
	});

	// ── Auto-refresh (device info — 30s countdown) ──────────────────

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

	// ── 1-second tick for live-updating relative timestamps ──

	let tick = $state(0);

	$effect(() => {
		if (!visible) return;
		const interval = setInterval(() => { tick++; }, 1000);
		return () => clearInterval(interval);
	});

	// ── Relay health polling (15s countdown when relay view visible) ──

	let relayCountdown = $state(15);

	$effect(() => {
		if (!visible || !isRelay) return;
		if (connectionStatus !== 'device_offline' && activeView !== 'relay') return;
		relayCountdown = 15;
		const interval = setInterval(() => {
			relayCountdown--;
			if (relayCountdown <= 0) {
				onrefreshrelayhealth?.();
				onrefreshrelayinfo?.();
				relayCountdown = 15;
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

	function signalColor(bars: number): string {
		if (bars >= 4) return 'text-green-400';
		if (bars >= 2) return 'text-amber-400';
		return 'text-red-400';
	}

	function gpsStatusDot(status: string): string {
		switch (status) {
			case 'active': return 'bg-green-500';
			case 'searching': return 'bg-yellow-500 animate-pulse';
			case 'error': return 'bg-red-500';
			default: return 'bg-neutral-600';
		}
	}

	function formatLogTime(ts: string): string {
		if (!ts) return '';
		// Parse as Date to display in local timezone (consistent with connection history)
		const d = new Date(ts);
		if (!isNaN(d.getTime())) {
			const hh = String(d.getHours()).padStart(2, '0');
			const mm = String(d.getMinutes()).padStart(2, '0');
			const ss = String(d.getSeconds()).padStart(2, '0');
			return `${hh}:${mm}:${ss}`;
		}
		// Logread format — just return first time-like pattern
		const timeMatch = ts.match(/(\d{2}:\d{2}:\d{2})/);
		return timeMatch ? timeMatch[1] : ts.slice(0, 8);
	}

	function logLevelColor(level: string): string {
		switch (level) {
			case 'error': return 'text-red-400/80';
			case 'warn': return 'text-amber-400/80';
			case 'debug': return 'text-neutral-700';
			default: return 'text-neutral-600';
		}
	}

	function formatTimeAgo(ts: number, _tick?: number): string {
		const secs = Math.floor((Date.now() - ts) / 1000);
		if (secs < 60) return `${secs}s ago`;
		if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
		if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
		return `${Math.floor(secs / 86400)}d ago`;
	}

	let probing = $state(false);

	// Clear probing state when probe result arrives
	$effect(() => {
		if (deviceProbe) probing = false;
	});

	function handleProbe() {
		probing = true;
		onprobedevice?.();
	}

	function eventLevelColor(level: ConnectionEvent['level']): string {
		switch (level) {
			case 'success': return 'text-green-400/80';
			case 'warn': return 'text-orange-400/80';
			case 'error': return 'text-red-400/80';
			default: return 'text-neutral-500';
		}
	}

	function eventLevelIcon(level: ConnectionEvent['level']): string {
		switch (level) {
			case 'success': return '+';
			case 'warn': return '!';
			case 'error': return 'x';
			default: return '-';
		}
	}

	function relativeTime(ts: number, _tick?: number): string {
		const delta = Date.now() - ts;
		if (delta < 1000) return 'now';
		if (delta < 60_000) return `${Math.floor(delta / 1000)}s`;
		if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m`;
		return `${Math.floor(delta / 3_600_000)}h`;
	}

	function dotColor(status: ConnectionStatus): string {
		switch (status) {
			case 'connected': return 'bg-green-500';
			case 'device_offline': return 'bg-orange-500 animate-pulse';
			case 'connecting': case 'reconnecting': return 'bg-yellow-500 animate-pulse';
			default: return 'bg-neutral-600';
		}
	}

	function statusLabel(status: ConnectionStatus): string {
		switch (status) {
			case 'connected': return 'connected';
			case 'device_offline': return 'device offline';
			case 'connecting': return 'connecting';
			case 'reconnecting': return 'reconnecting';
			default: return 'offline';
		}
	}

	function formatDuration(ms: number, _tick?: number): string {
		const totalSecs = Math.floor(Math.max(0, ms) / 1000);
		if (totalSecs < 60) return `${totalSecs}s`;
		const m = Math.floor(totalSecs / 60);
		const s = totalSecs % 60;
		if (m < 60) return `${m}m ${s}s`;
		const h = Math.floor(m / 60);
		const rm = m % 60;
		if (h < 24) return `${h}h ${rm}m`;
		const d = Math.floor(h / 24);
		const rh = h % 24;
		return `${d}d ${rh}h`;
	}

	// ── Diagnostics (derived from connection log) ─────────────────

	type InsightLevel = 'good' | 'warn' | 'critical';
	interface Insight { level: InsightLevel; text: string }

	let diagnostics = $derived.by(() => {
		void tick; // re-evaluate every second for live uptime
		const now = Date.now();
		let connects = 0;
		let reconnects = 0;
		let connectedMs = 0;
		let lastConnectTs: number | null = null;
		let lastDisconnectReason: string | null = null;
		let lastDisconnectTs: number | null = null;
		const reasons: Record<string, number> = {};

		for (const event of connectionLog) {
			if (event.message === 'connected' || event.message.startsWith('reconnected')) {
				if (event.message.startsWith('reconnected')) reconnects++;
				connects++;
				lastConnectTs = event.timestamp;
			} else if (
				event.message === 'device offline' ||
				event.message.startsWith('device disconnected:') ||
				event.message.startsWith('connection lost') ||
				event.message === 'disconnected'
			) {
				if (lastConnectTs) {
					connectedMs += event.timestamp - lastConnectTs;
					lastConnectTs = null;
				}
				if (event.message.startsWith('device disconnected:')) {
					const reason = event.message.slice('device disconnected: '.length);
					reasons[reason] = (reasons[reason] || 0) + 1;
					lastDisconnectReason = reason;
					lastDisconnectTs = event.timestamp;
				} else if (event.message === 'device offline') {
					const reason = event.detail?.startsWith('reason: ') ? event.detail.slice(8) : 'device offline';
					lastDisconnectReason = reason;
					lastDisconnectTs = event.timestamp;
					if (reason !== 'device offline') reasons[reason] = (reasons[reason] || 0) + 1;
				} else {
					lastDisconnectTs = event.timestamp;
				}
			}
		}

		// If currently connected, include time since last connect
		if (lastConnectTs && connectionStatus === 'connected') {
			connectedMs += now - lastConnectTs;
		}

		const firstEvent = connectionLog[0];
		const trackingMs = firstEvent ? now - firstEvent.timestamp : 0;
		const uptimePct = trackingMs > 0 ? Math.round((connectedMs / trackingMs) * 100) : 0;

		return { connects, reconnects, connectedMs, trackingMs, uptimePct, reasons, lastDisconnectReason, lastDisconnectTs };
	});

	let insights = $derived.by(() => {
		const result: Insight[] = [];
		const d = diagnostics;

		// Reconnect frequency
		if (d.reconnects > 5 && d.trackingMs < 3_600_000) {
			const mins = Math.max(1, Math.floor(d.trackingMs / 60_000));
			result.push({ level: 'critical', text: `${d.reconnects} reconnects in ${mins}m — very unstable` });
		} else if (d.reconnects > 3) {
			result.push({ level: 'warn', text: `${d.reconnects} reconnects — connection unstable` });
		}

		// Uptime
		if (d.trackingMs > 60_000) {
			if (d.uptimePct < 50) {
				result.push({ level: 'critical', text: `${d.uptimePct}% uptime — offline more than online` });
			} else if (d.uptimePct < 80) {
				result.push({ level: 'warn', text: `${d.uptimePct}% uptime` });
			}
		}

		// Heartbeat timeouts
		if (d.reasons['heartbeat timeout']) {
			result.push({ level: 'warn', text: 'heartbeat timeouts — network loss or process crash' });
		}

		// Relay RTT p95
		if (relayHealth?.tunnel.rtt_p95_ms != null && relayHealth.tunnel.rtt_p95_ms > 500) {
			result.push({ level: 'warn', text: `high relay latency: p95 ${relayHealth.tunnel.rtt_p95_ms}ms` });
		}

		// Dropped outbound
		if (relayHealth?.tunnel.dropped_outbound != null && relayHealth.tunnel.dropped_outbound > 0) {
			result.push({ level: 'warn', text: `${relayHealth.tunnel.dropped_outbound} dropped messages — relay congested` });
		}

		// Relay + tunnel status
		if (relayHealth) {
			if (relayHealth.status === 'ok' && !relayHealth.tunnel.connected) {
				result.push({ level: 'warn', text: 'relay healthy but device not on tunnel' });
			} else if (relayHealth.status === 'ok' && relayHealth.tunnel.connected) {
				result.push({ level: 'good', text: 'relay healthy, tunnel active' });
			}
		}

		// LTE signal (from relay or device info)
		const lte = relayHealth?.lte ?? deviceInfo?.lte;
		if (lte?.signal_bars != null) {
			if (lte.signal_bars <= 1) {
				result.push({ level: 'critical', text: `weak LTE (${lte.signal_bars}/5${lte.rsrp != null ? `, RSRP ${lte.rsrp}` : ''})` });
			} else if (lte.signal_bars <= 2) {
				result.push({ level: 'warn', text: `low LTE signal (${lte.signal_bars}/5)` });
			}
		}

		if (result.length === 0) {
			result.push({ level: 'good', text: 'no issues detected' });
		}

		return result;
	});

	// ── Connection history (from relay /api/health structured data) ──

	type TimelineEntry =
		| { kind: 'connected'; timeHHMM: string; durationLabel: string; reason: string | null; reasonColor: string; active: boolean }
		| { kind: 'offline'; timeHHMM: string; durationLabel: string };

	function fmtDuration(secs: number): string {
		const h = Math.floor(secs / 3600);
		const m = Math.floor((secs % 3600) / 60);
		const s = secs % 60;
		if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m ${String(s).padStart(2, '0')}s`;
		return `${m}m ${String(s).padStart(2, '0')}s`;
	}

	function epochToHHMM(epoch: number): string {
		const d = new Date(epoch * 1000);
		return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
	}

	function reasonColor(reason: string | null): string {
		if (!reason) return 'text-neutral-600';
		if (reason === 'heartbeat_timeout') return 'text-amber-400/80';
		if (reason === 'send_failed') return 'text-amber-400/80';
		if (reason === 'replaced') return 'text-neutral-500';
		if (reason === 'relay_shutdown') return 'text-neutral-600';
		if (reason === 'disconnected') return 'text-neutral-500';
		return 'text-neutral-600';
	}

	function reasonLabel(reason: string | null): string {
		if (!reason) return '';
		return reason.replace(/_/g, ' ');
	}

	interface HistorySummary {
		uptimePct: number;
		sessionCount: number;
		topReason: string | null;
		topReasonCount: number;
	}

	let connectionTimeline: TimelineEntry[] = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length) return [];

		const nowEpoch = Math.floor(Date.now() / 1000);
		const entries: TimelineEntry[] = [];

		// Sessions are chronological from the server — build timeline with gaps
		for (let i = 0; i < sessions.length; i++) {
			const s = sessions[i];

			// Insert offline gap between previous session's disconnect and this connect
			if (i > 0) {
				const prev = sessions[i - 1];
				const prevEnd = prev.disconnected_at ?? nowEpoch;
				const gapSecs = s.connected_at - prevEnd;
				if (gapSecs > 0) {
					entries.push({
						kind: 'offline',
						timeHHMM: epochToHHMM(prevEnd),
						durationLabel: fmtDuration(gapSecs),
					});
				}
			}

			const isActive = s.disconnected_at == null;
			const duration = isActive ? nowEpoch - s.connected_at : s.duration_secs;

			entries.push({
				kind: 'connected',
				timeHHMM: epochToHHMM(s.connected_at),
				durationLabel: fmtDuration(duration),
				reason: s.reason,
				reasonColor: reasonColor(s.reason),
				active: isActive,
			});
		}

		// Trailing offline gap: if the last session is disconnected, show the ongoing offline period
		if (sessions.length > 0) {
			const last = sessions[sessions.length - 1];
			if (last.disconnected_at != null) {
				const offlineSecs = nowEpoch - last.disconnected_at;
				if (offlineSecs > 0) {
					entries.push({
						kind: 'offline',
						timeHHMM: epochToHHMM(last.disconnected_at),
						durationLabel: fmtDuration(offlineSecs),
					});
				}
			}
		}

		return entries.reverse();
	});

	let historySummary: HistorySummary | null = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length) return null;

		const nowEpoch = Math.floor(Date.now() / 1000);
		const first = sessions[0];
		const last = sessions[sessions.length - 1];
		const totalSpan = nowEpoch - first.connected_at;
		if (totalSpan <= 0) return null;

		let connectedSecs = 0;
		const reasonCounts: Record<string, number> = {};

		for (const s of sessions) {
			const end = s.disconnected_at ?? nowEpoch;
			connectedSecs += end - s.connected_at;
			if (s.reason) {
				reasonCounts[s.reason] = (reasonCounts[s.reason] || 0) + 1;
			}
		}

		let topReason: string | null = null;
		let topReasonCount = 0;
		for (const [reason, count] of Object.entries(reasonCounts)) {
			if (count > topReasonCount) {
				topReason = reason;
				topReasonCount = count;
			}
		}

		return {
			uptimePct: Math.round((connectedSecs / totalSpan) * 1000) / 10,
			sessionCount: sessions.length,
			topReason,
			topReasonCount,
		};
	});
</script>

{#snippet systemMonitor(info: DeviceInfo)}
	<!-- System header -->
	<div class="space-y-0.5">
		<div class="flex items-baseline gap-2">
			<span class="text-sm text-neutral-200">{info.hostname}</span>
			<span class="text-[10px] text-neutral-600">{info.kernel}</span>
		</div>
		<div class="flex items-center gap-2 text-[10px]">
			<span class="text-neutral-500">up</span>
			<span class="text-green-400/70">{formatUptime(info.system_uptime_secs)}</span>
			<span class="text-neutral-700">|</span>
			<span class="text-neutral-600">{info.serial}</span>
		</div>
	</div>

	<!-- CPU -->
	<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
		<div class="flex items-baseline justify-between">
			<span class="text-[10px] text-neutral-600">[ cpu ]</span>
		</div>
		<div class="text-[10px] text-neutral-500 truncate">{info.cpu_model}</div>
		<div class="space-y-1">
			{#each [
				{ label: '1m', val: info.load_average[0] },
				{ label: '5m', val: info.load_average[1] },
				{ label: '15m', val: info.load_average[2] }
			] as { label, val }}
				{@const maxLoad = Math.max(4, ...info.load_average)}
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
	{@const memPct = pct(info.memory.used_bytes, info.memory.total_bytes)}
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
			{formatBytes(info.memory.used_bytes)} / {formatBytes(info.memory.total_bytes)}
			<span class="text-neutral-700">|</span>
			<span class="text-neutral-600">{formatBytes(info.memory.available_bytes)} free</span>
		</div>
	</div>

	<!-- Disk -->
	{@const diskPct = pct(info.disk.used_bytes, info.disk.total_bytes)}
	<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
		<div class="flex items-baseline justify-between">
			<span class="text-[10px] text-neutral-600">[ disk <span class="text-neutral-700">{info.disk.path}</span> ]</span>
			<span class="text-[10px] tabular-nums {usageTextColor(diskPct)}">{diskPct}%</span>
		</div>
		<div class="h-2 bg-neutral-800 rounded-full overflow-hidden">
			<div
				class="h-full rounded-full transition-all {usageColor(diskPct)}"
				style="width: {diskPct}%"
			></div>
		</div>
		<div class="text-[10px] text-neutral-500">
			{formatBytes(info.disk.used_bytes)} / {formatBytes(info.disk.total_bytes)}
			<span class="text-neutral-700">|</span>
			<span class="text-neutral-600">{formatBytes(info.disk.available_bytes)} free</span>
		</div>
	</div>

	<!-- Network -->
	{@const upInterfaces = info.interfaces.filter(i => { const s = i.state.toUpperCase(); return s === 'UP' || s === 'UNKNOWN'; })}
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
{/snippet}

{#snippet diagDisplay(d: ServerDiagnostics)}
	<!-- Process -->
	<div class="text-[10px] text-neutral-400 tabular-nums leading-relaxed">
		<span class="text-neutral-600">process:</span>
		pid {d.process.pid}
		<span class="text-neutral-700 mx-0.5">|</span>
		rss {formatBytes(d.process.rss_bytes)}
		<span class="text-neutral-700 mx-0.5">|</span>
		fds {d.process.open_fds}
		<span class="text-neutral-700 mx-0.5">|</span>
		threads {d.process.threads}
		<span class="text-neutral-700 mx-0.5">|</span>
		up {formatUptime(d.process.uptime_secs)}
	</div>

	<!-- System -->
	<div class="text-[10px] text-neutral-400 tabular-nums leading-relaxed">
		<span class="text-neutral-600">system:</span>
		load {d.system.load_avg.map((v: number) => v.toFixed(2)).join(' ')}
		<span class="text-neutral-700 mx-0.5">|</span>
		<span class="{d.system.memory.used_pct > 90 ? 'text-red-400/80' : d.system.memory.used_pct > 75 ? 'text-amber-400/80' : ''}">mem {d.system.memory.used_pct.toFixed(0)}%</span>
		({formatBytes(d.system.memory.total_bytes - d.system.memory.available_bytes)}/{formatBytes(d.system.memory.total_bytes)})
		{#if d.system.disk}
			<span class="text-neutral-700 mx-0.5">|</span>
			{@const diskPct = d.system.disk.total_bytes > 0 ? (d.system.disk.used_bytes / d.system.disk.total_bytes * 100) : 0}
			<span class="{diskPct > 90 ? 'text-red-400/80' : diskPct > 75 ? 'text-amber-400/80' : ''}">disk {diskPct.toFixed(0)}%</span>
			({formatBytes(d.system.disk.used_bytes)}/{formatBytes(d.system.disk.total_bytes)})
		{/if}
	</div>

	<!-- Network -->
	<div class="text-[10px] text-neutral-400 tabular-nums leading-relaxed">
		<span class="text-neutral-600">network:</span>
		{d.network.tcp.established} established
		<span class="text-neutral-700 mx-0.5">|</span>
		{d.network.tcp.listen} listen
		{#if d.network.tcp.time_wait > 0}
			<span class="text-neutral-700 mx-0.5">|</span>
			{d.network.tcp.time_wait} time_wait
		{/if}
		{#if d.network.tcp.close_wait > 0}
			<span class="text-neutral-700 mx-0.5">|</span>
			<span class="text-amber-400/80">{d.network.tcp.close_wait} close_wait</span>
		{/if}
	</div>

	<!-- Logs -->
	{#if d.logs.length > 0}
		<div class="border-t border-neutral-800/40 pt-2 mt-1">
			<div class="text-[9px] text-neutral-700 mb-1.5">
				── service logs (last 24h) ──
				{#if d.log_stats.errors > 0}
					<span class="text-red-400/80 ml-1">{d.log_stats.errors} error{d.log_stats.errors !== 1 ? 's' : ''}</span>
				{/if}
				{#if d.log_stats.warnings > 0}
					<span class="text-amber-400/80 ml-1">{d.log_stats.warnings} warning{d.log_stats.warnings !== 1 ? 's' : ''}</span>
				{/if}
			</div>
			<div class="max-h-48 overflow-y-auto space-y-0 scrollbar-thin scrollbar-thumb-neutral-800 scrollbar-track-transparent">
				{#each [...d.logs].reverse() as log}
					<div class="flex items-start gap-1.5 py-0.5 text-[10px] tabular-nums">
						<span class="shrink-0 text-neutral-700 w-14 text-right">{formatLogTime(log.timestamp)}</span>
						<span class="shrink-0 w-8 text-right {logLevelColor(log.level)}">{log.level}</span>
						<span class="text-neutral-400 break-all min-w-0">{log.message}</span>
					</div>
				{/each}
			</div>
		</div>
	{/if}
{/snippet}

<div class="flex flex-col h-full bg-neutral-950">
	<!-- Main two-column area -->
	<div class="flex flex-1 min-h-0 font-mono"
		 style:margin-right="{rightInset}px"
		 style:transition={rightInsetAnimate ? 'margin 300ms ease-in-out' : 'none'}>
		<!-- Left column: System Monitor / Relay Health -->
		<div class="w-1/2 border-r border-neutral-800/60 overflow-y-auto p-3 space-y-3">
			<!-- View toggle (only for relay connections) -->
			{#if isRelay}
				<div class="flex gap-1">
					<button
						class="px-2 py-0.5 text-[10px] rounded-full border transition-colors
							{activeView === 'device'
								? 'border-neutral-600 text-neutral-300 bg-neutral-800/50'
								: 'border-transparent text-neutral-600 hover:text-neutral-400'}
							disabled:opacity-30 disabled:cursor-not-allowed disabled:hover:text-neutral-600"
						disabled={connectionStatus === 'device_offline'}
						onclick={() => { activeView = 'device'; }}
					>device</button>
					<button
						class="px-2 py-0.5 text-[10px] rounded-full border transition-colors
							{activeView === 'relay'
								? 'border-neutral-600 text-neutral-300 bg-neutral-800/50'
								: 'border-transparent text-neutral-600 hover:text-neutral-400'}"
						onclick={() => { activeView = 'relay'; onrefreshrelayhealth?.(); relayCountdown = 15; }}
					>relay</button>
				</div>
			{/if}

			<!-- Device view -->
			{#if activeView === 'device'}
				{#if deviceInfo}
					{@render systemMonitor(deviceInfo)}
					<!-- LTE -->
					{#if deviceInfo.lte}
						{@const bars = deviceInfo.lte.signal_bars}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<div class="flex items-center justify-between">
								<div class="flex items-center gap-1.5">
									<span class="text-[10px] text-neutral-600">[ lte ]</span>
									{#if deviceInfo.lte.operator}
										<span class="text-[10px] text-neutral-300">{deviceInfo.lte.operator}</span>
									{/if}
									{#if deviceInfo.lte.technology}
										<span class="text-[10px] text-neutral-600">{deviceInfo.lte.technology}</span>
									{/if}
								</div>
								<div class="flex items-end gap-px">
									{#each [0, 1, 2, 3, 4] as i}
										<div
											class="w-1 rounded-sm {i < bars ? signalColor(bars).replace('text-', 'bg-') : 'bg-neutral-800'}"
											style="height: {4 + i * 2}px"
										></div>
									{/each}
								</div>
							</div>
							<div class="text-[10px] text-neutral-300 tabular-nums">
								{#if deviceInfo.lte.rsrp != null}
									<span>RSRP {deviceInfo.lte.rsrp}</span>
								{/if}
								<span class="text-neutral-700 mx-1">|</span>
								<span class="text-neutral-500">RSSI {deviceInfo.lte.rssi_dbm}</span>
								{#if deviceInfo.lte.sinr != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-500">SINR {deviceInfo.lte.sinr}</span>
								{/if}
								{#if deviceInfo.lte.rsrq != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-500">RSRQ {deviceInfo.lte.rsrq}</span>
								{/if}
								{#if deviceInfo.lte.band}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{deviceInfo.lte.band}</span>
								{/if}
							</div>
							{#if deviceInfo.lte.modem || deviceInfo.lte.cell_id}
								<div class="text-[10px] text-neutral-600 tabular-nums">
									{#if deviceInfo.lte.modem?.model}
										<span>{deviceInfo.lte.modem.model}</span>
									{/if}
									{#if deviceInfo.lte.modem?.imei}
										<span class="text-neutral-700 mx-1">|</span>
										<span>{deviceInfo.lte.modem.imei}</span>
									{/if}
									{#if deviceInfo.lte.cell_id}
										<span class="text-neutral-700 mx-1">|</span>
										<span>cell {deviceInfo.lte.cell_id}</span>
									{/if}
								</div>
							{/if}
							{#if deviceInfo.lte.modem?.iccid}
								<div class="text-[10px] text-neutral-700 tabular-nums">
									ICCID {deviceInfo.lte.modem.iccid}
								</div>
							{/if}
						</div>
					{/if}

					<!-- GPS -->
					{#if deviceInfo.gps}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<div class="flex items-center justify-between">
								<span class="text-[10px] text-neutral-600">[ gps ]</span>
								<div class="flex items-center gap-1.5">
									<span class="w-1.5 h-1.5 rounded-full shrink-0 {gpsStatusDot(deviceInfo.gps.status)}"></span>
									<span class="text-[10px] text-neutral-500">{deviceInfo.gps.status}</span>
								</div>
							</div>
							{#if deviceInfo.gps.latitude != null && deviceInfo.gps.longitude != null}
								<div class="text-[10px] text-neutral-300 tabular-nums">
									{deviceInfo.gps.latitude.toFixed(4)}, {deviceInfo.gps.longitude.toFixed(4)}
									{#if deviceInfo.gps.altitude != null}
										<span class="text-neutral-600 ml-1">alt {deviceInfo.gps.altitude.toFixed(0)}m</span>
									{/if}
								</div>
								<div class="text-[10px] text-neutral-500 tabular-nums">
									{#if deviceInfo.gps.satellites != null}
										<span>{deviceInfo.gps.satellites} sats</span>
									{/if}
									{#if deviceInfo.gps.hdop != null}
										<span class="text-neutral-700 mx-1">|</span>
										<span>hdop {deviceInfo.gps.hdop.toFixed(1)}</span>
									{/if}
									{#if deviceInfo.gps.fix_age_secs != null}
										<span class="text-neutral-700 mx-1">|</span>
										<span class="text-neutral-600">{deviceInfo.gps.fix_age_secs}s ago</span>
									{/if}
								</div>
							{/if}
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

				<!-- Server diagnostics (available for all connection types) -->
				{#if connectionStatus === 'connected'}
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-2">
						<div class="flex items-center justify-between">
							<span class="text-[10px] text-neutral-600">[ server diagnostics ]</span>
							<button
								class="text-[9px] text-neutral-600 hover:text-neutral-300 transition-colors px-1.5 py-0.5 border border-neutral-800/60 rounded"
								onclick={() => { diagLoading = true; onfetchdiagnostics?.(); }}
							>refresh</button>
						</div>

						{#if serverDiagnostics}
							{@render diagDisplay(serverDiagnostics)}
						{:else}
							<div class="text-[10px] text-neutral-600">
								click refresh to load server diagnostics
							</div>
						{/if}
					</div>
				{/if}

			<!-- Relay view -->
			{:else if activeView === 'relay'}
				<!-- Relay system monitor -->
				{#if relayInfo}
					{@render systemMonitor(relayInfo)}
				{:else if hasRelayApiKey}
					<div class="flex items-center justify-center h-16 text-[10px] text-neutral-600">
						loading relay system info...
					</div>
				{:else}
					<div class="text-[10px] text-neutral-600 border border-neutral-800/60 rounded p-2.5">
						add relay api key in server config to view relay system info
					</div>
				{/if}

				{#if relayHealth}
					<!-- Tunnel + device status (merged) -->
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
						<div class="flex items-center gap-2 text-[10px]">
							<span class="text-neutral-600">[ tunnel ]</span>
							<span class="w-1.5 h-1.5 rounded-full shrink-0 {relayHealth.tunnel.connected ? 'bg-green-500' : 'bg-orange-500'}"></span>
							<span class="{relayHealth.tunnel.connected ? 'text-neutral-400' : 'text-orange-400/80'}">{relayHealth.tunnel.connected ? 'device connected' : 'device offline'}</span>
							{#if relaySerial}
								<span class="text-neutral-700">|</span>
								<span class="text-neutral-600">{relaySerial}</span>
							{/if}
						</div>
						{#if relayHealth.tunnel.uptime_secs != null}
							<div class="text-[10px] text-neutral-500 tabular-nums">
								<span>up {formatUptime(relayHealth.tunnel.uptime_secs)}</span>
								{#if relayHealth.tunnel.rtt_median_ms != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span>rtt {relayHealth.tunnel.rtt_median_ms}ms{#if relayHealth.tunnel.rtt_p95_ms != null} <span class="text-neutral-700">(p95 {relayHealth.tunnel.rtt_p95_ms}ms)</span>{/if}</span>
								{/if}
								{#if relayHealth.tunnel.messages_sent != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{relayHealth.tunnel.messages_sent}&#8593; {relayHealth.tunnel.messages_received}&#8595;</span>
								{/if}
								{#if relayHealth.tunnel.reconnects > 0}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{relayHealth.tunnel.reconnects} reconnect{relayHealth.tunnel.reconnects !== 1 ? 's' : ''}</span>
								{/if}
							</div>
						{/if}
						{#if relayHealth.tunnel.dropped_outbound != null && relayHealth.tunnel.dropped_outbound > 0}
							<div class="text-[10px] text-amber-500/80 tabular-nums">
								{relayHealth.tunnel.dropped_outbound} dropped outbound
							</div>
						{/if}
						{#if !relayHealth.tunnel.connected}
							{#if disconnectReason}
								<div class="text-[10px]">
									<span class="text-neutral-600">reason:</span>
									<span class="text-orange-400/80 ml-1">{disconnectReason}</span>
									{#if lastConnectedAt}
										<span class="text-neutral-700 ml-1">({formatTimeAgo(lastConnectedAt, tick)})</span>
									{/if}
								</div>
							{/if}
							{#if relaySerial}
								<div class="flex items-center gap-2 pt-0.5">
									<button
										class="text-[9px] px-1.5 py-0.5 rounded border border-neutral-800 text-neutral-500 hover:text-neutral-300 hover:border-neutral-600 transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
										onclick={handleProbe}
										disabled={probing}
									>{probing ? 'checking...' : 'probe device'}</button>
									{#if deviceProbe}
										<span class="text-[10px] flex items-center gap-1">
											{#if deviceProbe.reachable}
												<span class="w-1.5 h-1.5 rounded-full bg-green-500 shrink-0"></span>
												<span class="text-green-400/80">reachable</span>
											{:else if deviceProbe.errorCode === 'DEVICE_NOT_FOUND'}
												<span class="w-1.5 h-1.5 rounded-full bg-orange-500 shrink-0"></span>
												<span class="text-orange-400/80">not registered</span>
											{:else if deviceProbe.errorCode === 'TIMEOUT'}
												<span class="w-1.5 h-1.5 rounded-full bg-amber-500 shrink-0"></span>
												<span class="text-amber-400/80">timed out</span>
											{:else if deviceProbe.errorCode === 'DEVICE_DISCONNECTED'}
												<span class="w-1.5 h-1.5 rounded-full bg-orange-500 shrink-0"></span>
												<span class="text-orange-400/80">disconnected</span>
											{:else if deviceProbe.errorCode === 'NETWORK_ERROR'}
												<span class="w-1.5 h-1.5 rounded-full bg-red-500 shrink-0"></span>
												<span class="text-red-400/80">relay unreachable</span>
											{:else}
												<span class="w-1.5 h-1.5 rounded-full bg-red-500 shrink-0"></span>
												<span class="text-red-400/80">failed</span>
											{/if}
											<span class="text-neutral-700 text-[9px]">{formatTimeAgo(deviceProbe.probedAt, tick)}</span>
										</span>
									{/if}
								</div>
							{/if}
						{/if}
					</div>

					<!-- Connection history (from relay structured data) -->
					{#if connectionTimeline.length > 0}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<span class="text-[10px] text-neutral-600">[ connection history ]</span>
							<div class="max-h-48 overflow-y-auto space-y-0 scrollbar-thin scrollbar-thumb-neutral-800 scrollbar-track-transparent">
								{#each connectionTimeline as entry}
									{#if entry.kind === 'connected'}
										<div class="flex items-center gap-1.5 py-0.5 text-[9px] tabular-nums">
											<span class="text-neutral-700 w-10 shrink-0 text-right">{entry.timeHHMM}</span>
											<span class="{entry.active ? 'text-green-400' : 'text-neutral-500'} w-16 shrink-0 text-right">{entry.durationLabel}</span>
											<span class="{entry.active ? 'text-green-400' : 'text-neutral-400'}">connected</span>
											{#if entry.reason}
												<span class="{entry.reasonColor}">{reasonLabel(entry.reason)}</span>
											{/if}
										</div>
									{:else}
										<div class="flex items-center gap-1.5 py-0.5 text-[9px] tabular-nums opacity-50">
											<span class="text-neutral-700 w-10 shrink-0 text-right">{entry.timeHHMM}</span>
											<span class="text-red-400/60 w-16 shrink-0 text-right">{entry.durationLabel}</span>
											<span class="text-red-400/60">offline</span>
										</div>
									{/if}
								{/each}
							</div>
							{#if historySummary}
								<div class="border-t border-neutral-800/40 pt-1 mt-1 flex items-center gap-1.5 text-[9px] text-neutral-600 tabular-nums">
									<span>uptime <span class="{historySummary.uptimePct > 90 ? 'text-green-400/70' : historySummary.uptimePct > 50 ? 'text-amber-400/70' : 'text-red-400/70'}">{historySummary.uptimePct}%</span></span>
									<span class="text-neutral-800">&middot;</span>
									<span>{historySummary.sessionCount} sessions</span>
									{#if historySummary.topReason}
										<span class="text-neutral-800">&middot;</span>
										<span>{historySummary.topReasonCount} {reasonLabel(historySummary.topReason)}</span>
									{/if}
								</div>
							{/if}
							{#if insights.length > 0}
								<div class="border-t border-neutral-800/40 pt-1 mt-1 space-y-0.5">
									{#each insights as insight}
										<div class="flex gap-1.5 text-[9px]">
											{#if insight.level === 'good'}
												<span class="text-green-400/80 shrink-0">+</span>
												<span class="text-green-400/70">{insight.text}</span>
											{:else if insight.level === 'warn'}
												<span class="text-orange-400/80 shrink-0">!</span>
												<span class="text-orange-400/70">{insight.text}</span>
											{:else}
												<span class="text-red-400/80 shrink-0">x</span>
												<span class="text-red-400/70">{insight.text}</span>
											{/if}
										</div>
									{/each}
								</div>
							{/if}
							{#if connectionStatus !== 'connected'}
								<div class="border-t border-neutral-800/40 pt-1.5 mt-1 space-y-0.5 text-[9px] text-neutral-700">
									<div>1. check device power and network</div>
									<div>2. verify sctl: <span class="text-neutral-600">ps | grep sctl</span></div>
									<div>3. check tunnel: <span class="text-neutral-600">cat /etc/sctl/sctl.toml</span></div>
									<div>4. check logs: <span class="text-neutral-600">logread | grep sctl</span></div>
								</div>
							{/if}
						</div>
					{:else if relayHealth}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<span class="text-[10px] text-neutral-600">[ connection history ]</span>
							<div class="text-[9px] text-neutral-700">no connection sessions recorded</div>
						</div>
					{/if}

					<!-- GPS (relay-side, if present) -->
					{#if relayHealth.gps}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<div class="flex items-center justify-between">
								<span class="text-[10px] text-neutral-600">[ gps ]</span>
								<div class="flex items-center gap-1.5">
									<span class="w-1.5 h-1.5 rounded-full shrink-0 {gpsStatusDot(relayHealth.gps.status)}"></span>
									<span class="text-[10px] text-neutral-500">{relayHealth.gps.status}</span>
								</div>
							</div>
							<div class="text-[10px] text-neutral-500 tabular-nums">
								{#if relayHealth.gps.satellites != null}
									<span>{relayHealth.gps.satellites} sats</span>
								{/if}
								{#if relayHealth.gps.fix_age_secs != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{relayHealth.gps.fix_age_secs}s ago</span>
								{/if}
							</div>
						</div>
					{/if}

					<!-- LTE (relay-side, if present) -->
					{#if relayHealth.lte}
						{@const bars = relayHealth.lte.signal_bars ?? 0}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<div class="flex items-center justify-between">
								<div class="flex items-center gap-1.5">
									<span class="text-[10px] text-neutral-600">[ lte ]</span>
									{#if relayHealth.lte.operator}
										<span class="text-[10px] text-neutral-300">{relayHealth.lte.operator}</span>
									{/if}
								</div>
								{#if bars > 0}
									<div class="flex items-end gap-px">
										{#each [0, 1, 2, 3, 4] as i}
											<div
												class="w-1 rounded-sm {i < bars ? signalColor(bars).replace('text-', 'bg-') : 'bg-neutral-800'}"
												style="height: {4 + i * 2}px"
											></div>
										{/each}
									</div>
								{/if}
							</div>
							<div class="text-[10px] text-neutral-300 tabular-nums">
								{#if relayHealth.lte.rsrp != null}
									<span>RSRP {relayHealth.lte.rsrp}</span>
								{/if}
								{#if relayHealth.lte.rssi_dbm != null}
									{#if relayHealth.lte.rsrp != null}<span class="text-neutral-700 mx-1">|</span>{/if}
									<span class="text-neutral-500">RSSI {relayHealth.lte.rssi_dbm}</span>
								{/if}
								{#if relayHealth.lte.sinr != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-500">SINR {relayHealth.lte.sinr}</span>
								{/if}
								{#if relayHealth.lte.band}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{relayHealth.lte.band}</span>
								{/if}
							</div>
						</div>
					{/if}

					<!-- Relay server diagnostics (relay VPS process/system/network/logs) -->
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-2">
						<div class="flex items-center justify-between">
							<span class="text-[10px] text-neutral-600">[ relay server ]</span>
							{#if hasRelayApiKey}
								<button
									class="text-[9px] text-neutral-600 hover:text-neutral-300 transition-colors px-1.5 py-0.5 border border-neutral-800/60 rounded"
									onclick={() => { relayDiagLoading = true; onfetchrelaydiagnostics?.(); }}
								>refresh</button>
							{/if}
						</div>

						{#if !hasRelayApiKey}
							<div class="text-[10px] text-neutral-600">
								add relay api key in server config to view relay diagnostics
							</div>
						{:else if relayDiagnostics}
							{@render diagDisplay(relayDiagnostics)}
						{:else if relayDiagLoading}
							<div class="text-[10px] text-neutral-600 animate-pulse">
								loading relay diagnostics...
							</div>
						{:else}
							<div class="text-[10px] text-neutral-600">
								no relay diagnostics available
							</div>
						{/if}
					</div>

					<!-- Device server diagnostics (through tunnel proxy) -->
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-2">
						<div class="flex items-center justify-between">
							<span class="text-[10px] text-neutral-600">[ device server ]</span>
							{#if connectionStatus === 'connected'}
								<button
									class="text-[9px] text-neutral-600 hover:text-neutral-300 transition-colors px-1.5 py-0.5 border border-neutral-800/60 rounded"
									onclick={() => { diagLoading = true; onfetchdiagnostics?.(); }}
								>refresh</button>
							{/if}
						</div>

						{#if connectionStatus !== 'connected'}
							<div class="text-[10px] text-neutral-600">
								device offline — diagnostics unavailable
							</div>
						{:else if serverDiagnostics}
							{@render diagDisplay(serverDiagnostics)}
						{:else}
							<div class="text-[10px] text-neutral-600">
								click refresh to load device diagnostics
							</div>
						{/if}
					</div>

				{:else}
					<div class="flex items-center justify-center h-32 text-[10px] text-neutral-600">
						loading relay info...
					</div>
				{/if}
			{/if}
		</div>

		<!-- Right column: Device Activity or Connection Log -->
		<div class="w-1/2 min-h-0 overflow-hidden">
			{#if activeView === 'relay'}
				<!-- Connection event log -->
				<div class="flex flex-col h-full bg-neutral-950 font-mono text-[11px]">
					<div class="flex items-center gap-2 px-3 py-2 border-b border-neutral-800 shrink-0">
						<span class="text-neutral-300 text-xs font-semibold">Connection Log</span>
						<span class="text-[9px] text-neutral-600 tabular-nums">{connectionLog.length}</span>
					</div>
					<div class="flex-1 overflow-y-auto min-h-0">
						{#each [...connectionLog].reverse() as event (event.id)}
							<div class="flex items-start gap-1 px-3 py-1 border-b border-neutral-800/20 hover:bg-neutral-800/40 transition-colors">
								<!-- Level icon -->
								<span class="w-3 text-center shrink-0 {eventLevelColor(event.level)}">{eventLevelIcon(event.level)}</span>
								<!-- Message + detail -->
								<div class="flex-1 min-w-0">
									<span class="text-neutral-400">{event.message}</span>
									{#if event.detail}
										<div class="text-[9px] text-neutral-600 truncate">{event.detail}</div>
									{/if}
								</div>
								<!-- Time -->
								<span class="shrink-0 text-neutral-600 tabular-nums text-[10px]">{relativeTime(event.timestamp, tick)}</span>
							</div>
						{/each}
						{#if connectionLog.length === 0}
							<div class="flex items-center justify-center py-8 text-neutral-600">
								No connection events yet
							</div>
						{/if}
					</div>
				</div>
			{:else}
				<HistoryViewer entries={activity} {restClient} {onOpenViewer} />
			{/if}
		</div>
	</div>

	<!-- Status bar (layout matches ControlBar: left content, spacer, right buttons) -->
	<div class="flex items-center px-1.5 py-1 bg-neutral-900 border-t border-neutral-800 text-[10px] text-neutral-500 h-7">
		<!-- Connection status (left) -->
		<span class="w-1.5 h-1.5 rounded-full shrink-0 {dotColor(connectionStatus)}"></span>
		<span class="text-neutral-600 ml-1.5">{statusLabel(connectionStatus)}</span>

		{#if isRelay}
			<span class="text-neutral-700 mx-1.5">&middot;</span>
			<span class="text-neutral-600">via relay</span>
			{#if relayHealth}
				<span class="text-neutral-700 ml-1">{relayHealth.tunnel.connected ? '' : '(no device)'}</span>
			{/if}
		{/if}

		<span class="text-neutral-700 mx-1.5">&middot;</span>

		<!-- Refresh countdown — device or relay depending on active view -->
		{#if connectionStatus === 'connected' && activeView === 'device'}
			<button
				class="tabular-nums text-neutral-600 hover:text-neutral-300 transition-colors"
				onclick={() => { onrefreshinfo?.(); refreshCountdown = 30; }}
				title="Refresh device info"
			>&#8635; {String(refreshCountdown).padStart(2, '0')}s</button>
		{:else if isRelay && (connectionStatus === 'device_offline' || activeView === 'relay')}
			<button
				class="tabular-nums text-neutral-600 hover:text-neutral-300 transition-colors"
				onclick={() => { onrefreshrelayhealth?.(); relayCountdown = 15; }}
				title="Refresh relay health"
			>&#8635; {String(relayCountdown).padStart(2, '0')}s</button>
		{:else if connectionStatus === 'connected' && activeView === 'relay'}
			<button
				class="tabular-nums text-neutral-600 hover:text-neutral-300 transition-colors"
				onclick={() => { onrefreshrelayhealth?.(); relayCountdown = 15; }}
				title="Refresh relay health"
			>&#8635; {String(relayCountdown).padStart(2, '0')}s</button>
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
						{connectionStatus !== 'connected' ? 'opacity-30 cursor-not-allowed' : sidePanelOpen && sidePanelTab === 'files' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
					disabled={connectionStatus !== 'connected'}
					onclick={onToggleFiles}
					title={connectionStatus !== 'connected' ? 'Device not connected' : 'Toggle file browser (Alt+E)'}
				>
					<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
					</svg>
				</button>
			{/if}
			{#if onTogglePlaybooks}
				<button
					class="w-5 h-5 flex items-center justify-center rounded transition-colors
						{connectionStatus !== 'connected' ? 'opacity-30 cursor-not-allowed' : sidePanelOpen && sidePanelTab === 'playbooks' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
					disabled={connectionStatus !== 'connected'}
					onclick={onTogglePlaybooks}
					title={connectionStatus !== 'connected' ? 'Device not connected' : 'Toggle playbooks (Alt+B)'}
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
