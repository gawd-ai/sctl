<script lang="ts">
	import type { ConnectionStatus, DeviceInfo, ActivityEntry, ViewerTab, RelayHealthInfo, DeviceProbeResult, ConnectionEvent, ServerDiagnostics, RelayConnectionSession, DeviceSnapshot, LteData } from '../types/terminal.types';
	import type { SctlRestClient } from '../utils/rest-client';
	import { earfcnInfo, rsrpColor, signalBgColor, unifiedBandOverview } from '../utils/lte';
	import type { UnifiedBandEntry } from '../utils/lte';
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
		telemetryLog: ConnectionEvent[];
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
		telemetryLog,
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

	// ── View toggle ─────────────────────────────────────────────────

	let activeView: DashboardView = $state('device');
	type DeviceRightTab = 'activity' | 'telemetry' | 'logs' | 'history';
	type RelayRightTab = 'connection' | 'logs' | 'history';
	let deviceRightTab: DeviceRightTab = $state('activity');
	let relayRightTab: RelayRightTab = $state('connection');
	let prevStatus: ConnectionStatus | null = $state(null);

	// Auto-fetch diagnostics when dashboard first becomes visible (device view)
	$effect(() => {
		if (visible && activeView === 'device' &&
			connectionStatus === 'connected' && !serverDiagnostics && !diagLoading) {
			diagLoading = true;
			onfetchdiagnostics?.();
		}
	});
	// Auto-fetch diagnostics when logs tab selected or dashboard visible (relay view)
	$effect(() => {
		if (visible && activeView === 'relay' &&
			hasRelayApiKey && !relayDiagnostics && !relayDiagLoading) {
			relayDiagLoading = true;
			onfetchrelaydiagnostics?.();
		}
	});

	// Auto-switch back to device view when it reconnects after being offline
	$effect(() => {
		if (connectionStatus === 'connected' && prevStatus === 'device_offline') {
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

	// ── LTE band management state ───────────────────────────────────

	let lteData = $state<LteData | null>(null);
	let lteBandAction: 'idle' | 'switching' | 'scanning' = $state('idle');
	let lteBandError: string | null = $state(null);
	let lteLoading = $state(false);
	let lteExpanded = $state(false);
	let lteConfirm: 'auto' | 'scan' | 'restore' | 'multi-lock' | number | null = $state(null);
	let lteSpeedTest = $state(false);
	let lteScanStartedAt: number | null = $state(null);
	// Multi-band selection
	let lteSelectionMode = $state(false);
	let lteSelectedBands: Set<number> = $state(new Set());

	/** Fetch full LTE data from /api/lte. */
	async function fetchLteData() {
		if (!restClient || lteLoading) return;
		lteLoading = true;
		try {
			lteData = await restClient.getLte();
			// If a scan was running and is now completed, clear scanning state
			if (lteBandAction === 'scanning' && lteData.scan_status?.state !== 'running') {
				lteBandAction = 'idle';
				lteScanStartedAt = null;
			}
		} catch {
			// Silently fail — device might be mid-scan and disconnected
		} finally {
			lteLoading = false;
		}
	}

	// Poll LTE data when panel is expanded and device has LTE
	$effect(() => {
		if (!visible || !lteExpanded || !deviceInfo?.lte || connectionStatus !== 'connected') return;
		// Fetch immediately
		fetchLteData();
		// Poll: 5s during scan, 30s otherwise
		const interval = setInterval(() => {
			fetchLteData();
		}, lteBandAction === 'scanning' ? 5000 : 30000);
		return () => clearInterval(interval);
	});

	/** Switch bands (auto or locked). */
	async function handleSetBands(mode: 'auto' | 'locked', bands?: number[], priorityBand?: number) {
		if (!restClient || lteBandAction !== 'idle') return;
		lteBandAction = 'switching';
		lteBandError = null;
		try {
			await restClient.setLteBands({ mode, bands, priority_band: priorityBand });
			await fetchLteData();
			onrefreshinfo?.();
		} catch (e) {
			lteBandError = e instanceof Error ? e.message : String(e);
		} finally {
			lteBandAction = 'idle';
		}
	}

	/** Start a band scan. */
	async function handleStartScan(bands?: number[], includeSpeedTest = false) {
		if (!restClient || lteBandAction !== 'idle') return;
		lteBandAction = 'scanning';
		lteBandError = null;
		lteScanStartedAt = Date.now();
		try {
			await restClient.startLteScan({ bands, include_speed_test: includeSpeedTest });
			// Start polling for scan progress
			await fetchLteData();
		} catch (e) {
			lteBandError = e instanceof Error ? e.message : String(e);
			lteBandAction = 'idle';
			lteScanStartedAt = null;
		}
	}

	/** Restore original bands from scan metadata. */
	async function handleRestoreBands() {
		const scan = lteData?.scan_status;
		if (!scan?.original_bands?.length) return;
		await handleSetBands('locked', scan.original_bands, scan.original_priority ?? undefined);
	}

	/** Handle clicking a band row. */
	function handleBandClick(entry: UnifiedBandEntry) {
		if (lteBandAction !== 'idle') return;
		if (lteSelectionMode) {
			if (!entry.lockable) return;
			const next = new Set(lteSelectedBands);
			if (next.has(entry.band)) next.delete(entry.band);
			else next.add(entry.band);
			lteSelectedBands = next;
			return;
		}
		const currentBands = deviceInfo?.lte?.band_config?.enabled_bands ?? [];
		const currentPriority = deviceInfo?.lte?.band_config?.priority_band;
		if (entry.enabled && !entry.serving) {
			// Drop this band from the lock set
			if (currentBands.length <= 1) return; // can't drop the last band
			if (lteConfirm === entry.band) {
				lteConfirm = null;
				const newBands = currentBands.filter(b => b !== entry.band);
				const priority = currentPriority === entry.band ? undefined : (currentPriority ?? undefined);
				handleSetBands('locked', newBands, priority);
			} else {
				lteConfirm = entry.band;
			}
		} else if (!entry.enabled && entry.lockable) {
			// Add this band to the lock set
			if (lteConfirm === entry.band) {
				lteConfirm = null;
				const newBands = [...new Set([...currentBands, entry.band])].sort((a, b) => a - b);
				handleSetBands('locked', newBands, currentPriority ?? undefined);
			} else {
				lteConfirm = entry.band;
			}
		}
	}

	/** Execute multi-band lock after two-click confirm. */
	function handleMultiBandLock() {
		if (lteConfirm === 'multi-lock') {
			const bands = [...lteSelectedBands].sort((a, b) => a - b);
			const currentPriority = deviceInfo?.lte?.band_config?.priority_band;
			const priority = currentPriority && lteSelectedBands.has(currentPriority) ? currentPriority : undefined;
			lteConfirm = null;
			lteSelectionMode = false;
			lteSelectedBands = new Set();
			handleSetBands('locked', bands, priority);
		} else {
			lteConfirm = 'multi-lock';
		}
	}

	function enterSelectionMode() {
		lteSelectionMode = true;
		lteConfirm = null;
		// Pre-populate with currently enabled bands
		const enabled = deviceInfo?.lte?.band_config?.enabled_bands ?? [];
		lteSelectedBands = new Set(enabled);
	}

	function cancelSelectionMode() {
		lteSelectionMode = false;
		lteSelectedBands = new Set();
		lteConfirm = null;
	}

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
			const mon = d.getMonth() + 1;
			const day = d.getDate();
			const hh = String(d.getHours()).padStart(2, '0');
			const mm = String(d.getMinutes()).padStart(2, '0');
			const ss = String(d.getSeconds()).padStart(2, '0');
			return `${mon}/${day} ${hh}:${mm}:${ss}`;
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

	// On a relay, tunnel.connected is always false (relay isn't a tunnel client).
	// Derive device connectivity from connection_history (disconnected_at === null means live).
	let relayDeviceConnected = $derived.by(() => {
		if (!relayHealth?.connection_history) return relayHealth?.tunnel.connected ?? false;
		return relayHealth.connection_history.some(
			(s) => s.disconnected_at === null && s.serial === relaySerial
		);
	});

	// Device snapshot for the connected serial (relay mode)
	let offlineSnapshot = $derived.by(() => {
		if (!relayHealth?.device_snapshots || !relaySerial) return null;
		return relayHealth.device_snapshots[relaySerial] ?? null;
	});

	/** Derived: watchdog info from relay snapshot or relay health. */
	let watchdogInfo = $derived(offlineSnapshot?.last_watchdog ?? relayHealth?.device_snapshots?.[relaySerial ?? '']?.last_watchdog ?? null);

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

	function formatEventTime(epochMs: number): string {
		const d = new Date(epochMs);
		const mon = d.getMonth() + 1;
		const day = d.getDate();
		const hh = String(d.getHours()).padStart(2, '0');
		const mm = String(d.getMinutes()).padStart(2, '0');
		const ss = String(d.getSeconds()).padStart(2, '0');
		return `${mon}/${day} ${hh}:${mm}:${ss}`;
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
			if (relayHealth.status === 'ok' && !relayDeviceConnected) {
				result.push({ level: 'warn', text: 'relay healthy but device not on tunnel' });
			} else if (relayHealth.status === 'ok' && relayDeviceConnected) {
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
		| { kind: 'connected'; serial?: string; time: string; durationLabel: string; reason: string | null; reasonColor: string; active: boolean }
		| { kind: 'offline'; time: string; durationLabel: string };

	function fmtDuration(secs: number): string {
		const h = Math.floor(secs / 3600);
		const m = Math.floor((secs % 3600) / 60);
		const s = secs % 60;
		if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m ${String(s).padStart(2, '0')}s`;
		return `${m}m ${String(s).padStart(2, '0')}s`;
	}

	function epochToTimeStr(epoch: number): string {
		const d = new Date(epoch * 1000);
		const mon = d.getMonth() + 1;
		const day = d.getDate();
		const hh = String(d.getHours()).padStart(2, '0');
		const mm = String(d.getMinutes()).padStart(2, '0');
		const ss = String(d.getSeconds()).padStart(2, '0');
		return `${mon}/${day} ${hh}:${mm}:${ss}`;
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

	function buildTimeline(sessions: RelayConnectionSession[], includeSerial: boolean): TimelineEntry[] {
		if (!sessions.length) return [];

		const nowEpoch = Math.floor(Date.now() / 1000);
		const entries: TimelineEntry[] = [];

		for (let i = 0; i < sessions.length; i++) {
			const s = sessions[i];

			if (i > 0) {
				const prev = sessions[i - 1];
				const prevEnd = prev.disconnected_at ?? nowEpoch;
				const gapSecs = s.connected_at - prevEnd;
				if (gapSecs > 0) {
					entries.push({
						kind: 'offline',
						time: epochToTimeStr(prevEnd),
						durationLabel: fmtDuration(gapSecs),
					});
				}
			}

			const isActive = s.disconnected_at == null;
			const duration = isActive ? nowEpoch - s.connected_at : s.duration_secs;

			entries.push({
				kind: 'connected',
				serial: includeSerial ? s.serial : undefined,
				time: epochToTimeStr(s.connected_at),
				durationLabel: fmtDuration(duration),
				reason: s.reason,
				reasonColor: reasonColor(s.reason),
				active: isActive,
			});
		}

		if (sessions.length > 0) {
			const last = sessions[sessions.length - 1];
			if (last.disconnected_at != null) {
				const offlineSecs = nowEpoch - last.disconnected_at;
				if (offlineSecs > 0) {
					entries.push({
						kind: 'offline',
						time: epochToTimeStr(last.disconnected_at),
						durationLabel: fmtDuration(offlineSecs),
					});
				}
			}
		}

		return entries.reverse();
	}

	function buildHistorySummary(sessions: RelayConnectionSession[]): HistorySummary | null {
		if (!sessions.length) return null;

		const nowEpoch = Math.floor(Date.now() / 1000);
		const first = sessions[0];
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
	}

	// Device history: filtered to relaySerial
	let deviceHistoryTimeline: TimelineEntry[] = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length || !relaySerial) return [];
		return buildTimeline(sessions.filter(s => s.serial === relaySerial), false);
	});

	let deviceHistorySummary: HistorySummary | null = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length || !relaySerial) return null;
		return buildHistorySummary(sessions.filter(s => s.serial === relaySerial));
	});

	// Relay history: all serials
	let relayHistoryTimeline: TimelineEntry[] = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length) return [];
		return buildTimeline(sessions, true);
	});

	let relayHistorySummary: HistorySummary | null = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length) return null;
		return buildHistorySummary(sessions);
	});

	// ── Multi-device tunnel list (from connection_history) ───────────

	interface TunnelDevice {
		serial: string;
		connected: boolean;
		durationLabel: string;
		lastSeenLabel: string | null;
		isCurrentDevice: boolean;
	}

	let tunnelDevices: TunnelDevice[] = $derived.by(() => {
		void tick;
		const sessions = relayHealth?.connection_history;
		if (!sessions?.length) return [];

		const nowEpoch = Math.floor(Date.now() / 1000);
		const deviceMap = new Map<string, { connected: boolean; connectedAt: number; lastSeen: number }>();

		for (const s of sessions) {
			const isActive = s.disconnected_at == null;
			const existing = deviceMap.get(s.serial);

			if (!existing || isActive || s.connected_at > existing.connectedAt) {
				const snapshot = relayHealth?.device_snapshots?.[s.serial];
				deviceMap.set(s.serial, {
					connected: isActive,
					connectedAt: s.connected_at,
					lastSeen: snapshot?.last_seen ?? (s.disconnected_at ?? s.connected_at),
				});
			}
		}

		return [...deviceMap.entries()]
			.map(([serial, info]) => ({
				serial,
				connected: info.connected,
				durationLabel: info.connected ? fmtDuration(nowEpoch - info.connectedAt) : '',
				lastSeenLabel: !info.connected ? formatTimeAgo(info.lastSeen * 1000, tick) : null,
				isCurrentDevice: serial === relaySerial,
			}))
			.sort((a, b) => {
				if (a.connected !== b.connected) return a.connected ? -1 : 1;
				return a.serial.localeCompare(b.serial);
			});
	});

	function copySerial(serial: string) {
		navigator.clipboard.writeText(serial);
	}
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

{#snippet serverInfo(label: string, d: ServerDiagnostics | null | undefined, loading: boolean, onrefresh?: () => void)}
	<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
		<div class="flex items-center justify-between">
			<span class="text-[10px] text-neutral-600">[ {label} ]</span>
			{#if onrefresh}
				<button
					class="text-[9px] text-neutral-700 hover:text-neutral-400 transition-colors disabled:opacity-40"
					onclick={onrefresh}
					disabled={loading}
				>{loading ? '...' : 'refresh'}</button>
			{/if}
		</div>
		{#if d}
			<div class="text-[10px] text-neutral-400 tabular-nums">
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
			<div class="text-[10px] text-neutral-500 tabular-nums">
				tcp: {d.network.tcp.established} established, {d.network.tcp.listen} listen{#if d.network.tcp.time_wait > 0}, {d.network.tcp.time_wait} time_wait{/if}{#if d.network.tcp.close_wait > 0}, <span class="text-amber-400/80">{d.network.tcp.close_wait} close_wait</span>{/if}
			</div>
		{:else if loading}
			<div class="text-[10px] text-neutral-700 animate-pulse">loading...</div>
		{/if}
	</div>
{/snippet}

{#snippet serviceLogs(d: ServerDiagnostics)}
	{#each [...d.logs].reverse() as log}
		<div class="flex items-start gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 hover:bg-neutral-800/40 transition-colors">
			<span class="shrink-0 text-neutral-600 w-20 text-right">{formatLogTime(log.timestamp)}</span>
			<span class="shrink-0 w-8 text-right {logLevelColor(log.level)}">{log.level}</span>
			<span class="text-neutral-400 break-all min-w-0">{log.message}</span>
		</div>
	{/each}
{/snippet}

{#snippet eventLog(events: ConnectionEvent[])}
	{#each [...events].reverse() as event (event.id)}
		<div class="flex items-start gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 hover:bg-neutral-800/40 transition-colors">
			<span class="shrink-0 text-neutral-600 w-20 text-right">{formatEventTime(event.timestamp)}</span>
			<span class="w-3 text-center shrink-0 {eventLevelColor(event.level)}">{eventLevelIcon(event.level)}</span>
			<span class="text-neutral-400 break-all min-w-0">{event.message}{#if event.detail} <span class="text-neutral-600">{event.detail}</span>{/if}</span>
		</div>
	{/each}
{/snippet}

{#snippet tabButton(label: string, active: boolean, onclick: () => void, count?: number)}
	<button
		class="px-2 py-0.5 text-[10px] rounded-full border transition-colors
			{active
				? 'border-neutral-600 text-neutral-300 bg-neutral-800/50'
				: 'border-transparent text-neutral-600 hover:text-neutral-400'}"
		{onclick}
	>{label}{#if count != null}<span class="text-neutral-700 tabular-nums ml-1">{count}</span>{/if}</button>
{/snippet}

{#snippet tabHeader(title: string, subtitle?: string, onrefresh?: () => void, refreshLoading?: boolean)}
	<div class="flex items-center gap-2 px-3 h-8 border-b border-neutral-800/50 shrink-0">
		<span class="text-neutral-400 text-[10px]">{title}</span>
		{#if subtitle}
			<span class="text-[10px] text-neutral-600 tabular-nums">{subtitle}</span>
		{/if}
		<div class="flex-1"></div>
		{#if onrefresh}
			<button
				class="text-[10px] text-neutral-600 hover:text-neutral-300 transition-colors px-1.5 py-0.5 border border-neutral-800/60 rounded disabled:opacity-40"
				onclick={onrefresh}
				disabled={refreshLoading}
			>{refreshLoading ? 'loading...' : 'refresh'}</button>
		{/if}
	</div>
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
								: 'border-transparent text-neutral-600 hover:text-neutral-400'}"
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
						{@const lte = deviceInfo.lte}
						{@const bars = lte.signal_bars}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
							<!-- Header: operator, tech, signal bars, expand toggle -->
							<div class="flex items-center justify-between">
								<div class="flex items-center gap-1.5">
									<button
										class="text-[10px] text-neutral-600 hover:text-neutral-400 transition-colors"
										onclick={() => { lteExpanded = !lteExpanded; lteConfirm = null; lteSpeedTest = false; }}
									>[ lte {lteExpanded ? '−' : '+'} ]</button>
									{#if lte.operator}
										<span class="text-[10px] text-neutral-300">{lte.operator}</span>
									{/if}
									{#if lte.technology}
										<span class="text-[10px] text-neutral-600">{lte.technology}</span>
									{/if}
								</div>
								<div class="flex items-center gap-2">
									{#if lteBandAction === 'switching'}
										<span class="text-[9px] text-amber-400 animate-pulse">switching...</span>
									{:else if lteBandAction === 'scanning'}
										<span class="text-[9px] text-cyan-400 animate-pulse">scanning...</span>
									{/if}
									<div class="flex items-end gap-px">
										{#each [0, 1, 2, 3, 4] as i}
											<div
												class="w-1 rounded-sm {i < bars ? signalBgColor(bars) : 'bg-neutral-800'}"
												style="height: {4 + i * 2}px"
											></div>
										{/each}
									</div>
								</div>
							</div>

							<!-- Signal metrics -->
							<div class="text-[10px] text-neutral-300 tabular-nums">
								{#if lte.rsrp != null}
									<span class={rsrpColor(lte.rsrp)}>RSRP {lte.rsrp}</span>
								{/if}
								<span class="text-neutral-700 mx-1">|</span>
								<span class="text-neutral-500">RSSI {lte.rssi_dbm}</span>
								{#if lte.sinr != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-500">SINR {lte.sinr}</span>
								{/if}
								{#if lte.rsrq != null}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-500">RSRQ {lte.rsrq}</span>
								{/if}
								{#if lte.band}
									<span class="text-neutral-700 mx-1">|</span>
									<span class="text-neutral-600">{lte.band}</span>
								{/if}
							</div>

							<!-- Cell details -->
							{#if lte.freq_band != null || lte.pci != null || lte.earfcn != null || lte.enodeb_id != null || lte.tac}
								<div class="text-[10px] text-neutral-500 tabular-nums">
									{#if lte.freq_band != null}
										<span class="text-neutral-300">B{lte.freq_band}</span>
									{/if}
									{#if lte.duplex}
										<span class="text-neutral-600 ml-0.5">{lte.duplex}</span>
									{/if}
									{#if lte.dl_bw_mhz}
										<span class="text-neutral-600 ml-0.5">{lte.dl_bw_mhz}MHz</span>
									{/if}
									{#if lte.pci != null}
										<span class="text-neutral-700 mx-1">|</span>
										<span>PCI {lte.pci}</span>
									{/if}
									{#if lte.earfcn != null}
										{@const ei = earfcnInfo(lte.earfcn)}
										<span class="text-neutral-700 mx-1">|</span>
										<span>EARFCN {lte.earfcn}{#if ei} ({ei.frequencyMhz} MHz){/if}</span>
									{/if}
									{#if lte.enodeb_id != null}
										<span class="text-neutral-700 mx-1">|</span>
										<span>eNB {lte.enodeb_id}</span>
										{#if lte.sector != null}
											<span class="text-neutral-600 ml-0.5">s{lte.sector}</span>
										{/if}
									{/if}
									{#if lte.tac}
										<span class="text-neutral-700 mx-1">|</span>
										<span>TAC {lte.tac}</span>
									{/if}
								</div>
							{/if}
							{#if lte.connection_state || lte.plmn || (lte.ul_bw_mhz && lte.dl_bw_mhz)}
								<div class="text-[10px] text-neutral-600 tabular-nums">
									{#if lte.connection_state}
										<span class="text-neutral-500">{lte.connection_state}</span>
									{/if}
									{#if lte.plmn}
										<span class="text-neutral-700 mx-1">|</span>
										<span>PLMN {lte.plmn}</span>
									{/if}
									{#if lte.ul_bw_mhz && lte.dl_bw_mhz}
										<span class="text-neutral-700 mx-1">|</span>
										<span>UL {lte.ul_bw_mhz} / DL {lte.dl_bw_mhz} MHz</span>
									{/if}
								</div>
							{/if}
							{#if lte.modem || lte.cell_id}
								<div class="text-[10px] text-neutral-600 tabular-nums">
									{#if lte.modem?.model}
										<span>{lte.modem.model}</span>
									{/if}
									{#if lte.modem?.imei}
										<span class="text-neutral-700 mx-1">|</span>
										<span>{lte.modem.imei}</span>
									{/if}
									{#if lte.cell_id}
										<span class="text-neutral-700 mx-1">|</span>
										<span>cell {lte.cell_id}</span>
									{/if}
								</div>
							{/if}
							{#if lte.modem?.iccid}
								<div class="text-[10px] text-neutral-700 tabular-nums">
									ICCID {lte.modem.iccid}
								</div>
							{/if}

							<!-- Collapsed summary line -->
							{#if !lteExpanded && lte.band_config}
								{@const bc = lte.band_config}
								{@const isAuto = bc.enabled_bands.length >= 20}
								<div class="text-[10px] tabular-nums">
									{#if isAuto}
										<span class="text-neutral-500">auto ({bc.enabled_bands.length} bands)</span>
									{:else}
										<span class="text-amber-400/80">locked → B{bc.enabled_bands.join(', B')}</span>
									{/if}
									{#if lte.freq_band != null}
										<span class="text-neutral-700 mx-1"> </span>
										<span class="text-green-400/80">serving B{lte.freq_band}</span>
									{/if}
									{#if lteBandAction === 'scanning'}
										<span class="text-neutral-700 mx-1"> </span>
										<span class="text-cyan-400 animate-pulse">scan active</span>
									{/if}
								</div>
							{/if}

							<!-- ═══ Expanded LTE panel ═══ -->
							{#if lteExpanded}
								<!-- Band control bar -->
								<!-- svelte-ignore a11y_no_static_element_interactions -->
								<div class="border-t border-neutral-800/40 pt-1.5 mt-1.5" onmouseleave={() => { lteConfirm = null; lteSpeedTest = false; }}>
									<div class="flex items-center gap-1.5 flex-wrap">
										<!-- Mode indicator -->
										{#if lte.band_config}
											{@const bc = lte.band_config}
											{@const isAuto = bc.enabled_bands.length >= 20}
											<span class="text-[9px] {isAuto ? 'text-neutral-500' : 'text-amber-400/80'}">
												{isAuto ? 'mode: auto' : `locked → B${bc.enabled_bands.join(', B')}`}
												{#if bc.priority_band}
													<span class="text-neutral-600">pri B{bc.priority_band}</span>
												{/if}
											</span>
										{/if}
										<div class="flex-1"></div>
										<!-- Action buttons -->
										{#if lteConfirm === 'scan'}
											<label class="flex items-center gap-1 text-[9px] text-neutral-500 cursor-pointer select-none">
												<input type="checkbox" bind:checked={lteSpeedTest} class="w-3 h-3 accent-cyan-500" />
												speed test
											</label>
										{/if}
										<button
											class="px-1.5 py-0.5 text-[9px] rounded border transition-colors
												{lteBandAction !== 'idle'
													? 'border-neutral-800 text-neutral-700 cursor-not-allowed'
													: lteConfirm === 'auto'
														? 'border-amber-600 text-amber-400 bg-amber-500/10'
														: 'border-neutral-700 text-neutral-400 hover:text-neutral-200 hover:border-neutral-600'}"
											disabled={lteBandAction !== 'idle'}
											onclick={() => {
												if (lteConfirm === 'auto') { lteConfirm = null; handleSetBands('auto'); }
												else { lteConfirm = 'auto'; }
											}}
										>{lteConfirm === 'auto' ? 'auto? ⚠' : 'auto'}</button>
										<button
											class="px-1.5 py-0.5 text-[9px] rounded border transition-colors
												{lteBandAction !== 'idle'
													? 'border-neutral-800 text-neutral-700 cursor-not-allowed'
													: lteConfirm === 'scan'
														? 'border-red-600 text-red-400 bg-red-500/10'
														: 'border-neutral-700 text-neutral-400 hover:text-neutral-200 hover:border-neutral-600'}"
											disabled={lteBandAction !== 'idle'}
											onclick={() => {
												if (lteConfirm === 'scan') { lteConfirm = null; handleStartScan(undefined, lteSpeedTest); lteSpeedTest = false; }
												else { lteConfirm = 'scan'; }
											}}
										>{lteConfirm === 'scan' ? 'scan? drops conn' : 'scan'} {#if lteConfirm !== 'scan'}<span class="text-amber-500/70">!</span>{/if}</button>
									</div>
									{#if lteBandError}
										<div class="text-[9px] text-red-400 mt-1">{lteBandError}</div>
									{/if}
								</div>

								<!-- LTE errors from /api/lte -->
								{#if lteData?.last_error}
									<div class="text-[9px] text-amber-400/80 mt-1">
										⚠ last AT error: {lteData.last_error}{#if lteData.errors_total > 1} ({lteData.errors_total} total errors){/if}
									</div>
								{/if}

								<!-- Unified band list -->
								{#if lte.band_config}
									{@const enabledCount = lte.band_config.enabled_bands.length}
									{@const completedScanResults = lteData?.scan_status?.state === 'completed' ? (lteData.scan_status.results ?? []) : []}
									{@const unified = unifiedBandOverview({
										enabledBands: lte.band_config.enabled_bands,
										priorityBand: lte.band_config.priority_band,
										servingBand: lte.freq_band,
										servingRsrp: lte.rsrp,
										neighbors: lte.neighbors ?? [],
										bandHistory: lteData?.band_history ?? [],
										scanResults: completedScanResults,
									})}
									<!-- svelte-ignore a11y_no_static_element_interactions -->
									<div class="text-[10px] tabular-nums mt-1" onmouseleave={() => { if (typeof lteConfirm === 'number') lteConfirm = null; }}>
										<div class="flex items-center gap-1.5">
											<span class="text-neutral-700">bands ({unified.length})</span>
											{#if lteSelectionMode}
												<span class="text-neutral-400 text-[9px]">{lteSelectedBands.size} selected</span>
												<div class="flex-1"></div>
												<button
													class="text-[9px] text-neutral-600 hover:text-neutral-300 transition-colors"
													onclick={cancelSelectionMode}
												>cancel</button>
												{#if lteSelectedBands.size > 0}
													<button
														class="text-[9px] px-1.5 py-0.5 rounded border transition-colors
															{lteConfirm === 'multi-lock'
																? 'text-amber-400 border-amber-600 bg-amber-500/10'
																: 'text-neutral-400 border-neutral-700 hover:text-neutral-200 hover:border-neutral-600'}"
														disabled={lteBandAction !== 'idle'}
														onclick={handleMultiBandLock}
													>{lteConfirm === 'multi-lock' ? `lock ${lteSelectedBands.size}? ⚠` : `lock ${lteSelectedBands.size} →`}</button>
												{/if}
											{:else}
												<div class="flex-1"></div>
												<button
													class="text-[9px] text-neutral-600 hover:text-neutral-400 transition-colors"
													onclick={enterSelectionMode}
												>select</button>
											{/if}
										</div>
										{#each unified as entry}
											{@const canDrop = entry.enabled && !entry.serving && enabledCount > 1}
											{@const canAdd = !entry.enabled && entry.lockable}
											{@const isClickable = lteSelectionMode ? entry.lockable : (canDrop || canAdd)}
											{@const isConfirming = !lteSelectionMode && lteConfirm === entry.band}
											{@const isSelected = lteSelectionMode && lteSelectedBands.has(entry.band)}
											<button
												class="flex items-center w-full pl-2 py-0.5 rounded transition-colors
													{entry.serving ? '' : entry.enabled ? '' : entry.lockable ? 'opacity-60' : 'opacity-25'}
													{!isClickable ? 'cursor-default' : lteBandAction !== 'idle' ? 'cursor-not-allowed' : isConfirming ? (canDrop ? 'bg-red-500/10' : 'bg-amber-500/10') : isSelected ? 'bg-neutral-800/60' : 'hover:bg-neutral-800/50 cursor-pointer'}"
												disabled={lteBandAction !== 'idle' || !isClickable}
												onclick={() => handleBandClick(entry)}
												title={!entry.lockable ? `B${entry.band} — no registration` : canDrop ? `Remove B${entry.band} from lock set` : canAdd ? `Add B${entry.band} to lock set` : ''}
											>
												<span class="w-7 text-left {entry.serving ? 'text-green-400' : entry.enabled ? 'text-neutral-400' : entry.lockable ? 'text-neutral-500' : 'text-neutral-700'}">B{entry.band}</span>
												{#if entry.frequencyMhz != null}
													<span class="text-neutral-600 w-10 text-right">{entry.frequencyMhz}</span>
												{:else}
													<span class="w-10"></span>
												{/if}
												{#if entry.rsrp != null}
													<span class="{rsrpColor(entry.rsrp)} ml-2 w-8 text-right">{entry.rsrp}</span>
													{#if entry.rsrpSource && entry.rsrpSource !== 'serving'}
														<span class="text-neutral-700 text-[8px] ml-0.5">{entry.rsrpSource === 'scan' ? 'scan' : entry.rsrpSource === 'history' ? 'hist' : 'nbr'}</span>
													{/if}
												{:else}
													<span class="text-neutral-700 ml-2 w-8 text-right">&mdash;</span>
												{/if}
												{#if entry.serving}
													<span class="text-green-400/60 ml-1.5">serving</span>
												{/if}
												{#if entry.priority}
													<span class="text-neutral-600 ml-1">pri</span>
												{/if}
												{#if entry.cellCount > 0}
													<span class="text-neutral-600 ml-1.5">{entry.cellCount}c</span>
												{/if}
												{#if entry.scan?.downloadBps != null}
													<span class="text-neutral-500 ml-1.5">{(entry.scan.downloadBps / 1_000_000).toFixed(1)}Mbps</span>
												{/if}
												<!-- Right edge: context-dependent -->
												{#if lteSelectionMode}
													<span class="ml-auto text-[9px] {isSelected ? 'text-neutral-300' : 'text-neutral-700'}">{isSelected ? '[x]' : entry.lockable ? '[ ]' : ''}</span>
												{:else if isConfirming && canDrop}
													<span class="ml-auto text-red-400 text-[9px]">drop?</span>
												{:else if isConfirming && canAdd}
													<span class="ml-auto text-amber-400 text-[9px]">add?</span>
												{:else if entry.history}
													<span class="text-neutral-700 ml-auto text-[9px]">
														{#if entry.history.trend === 'up'}▲{:else if entry.history.trend === 'down'}▼{/if}
														{entry.history.observationCount}x
													</span>
												{:else if canAdd}
													<span class="ml-auto text-neutral-700 text-[9px]">+ add</span>
												{/if}
											</button>
										{/each}
									</div>
								{/if}

								<!-- Scan progress bar (stays separate) -->
								{#if lteData?.scan_status?.state === 'running' || (lteBandAction === 'scanning' && connectionStatus !== 'connected')}
									{@const scan = lteData?.scan_status}
									<div class="border-t border-neutral-800/40 pt-1.5 mt-1.5 space-y-1">
										{#if lteBandAction === 'scanning' && connectionStatus !== 'connected'}
											<div class="text-[10px]">
												<div class="flex items-center gap-1.5">
													<span class="text-cyan-400 animate-pulse">scan in progress</span>
												</div>
												<div class="h-1 bg-neutral-800 rounded-full mt-1 overflow-hidden">
													<div class="h-full bg-cyan-500/30 rounded-full animate-pulse" style="width: 100%"></div>
												</div>
												<div class="text-[9px] text-neutral-600 mt-0.5">
													{#if lteScanStartedAt}
														started {formatTimeAgo(lteScanStartedAt, tick)} — connection will resume when complete
														{@const bandsCount = scan?.bands_to_scan?.length ?? lte.band_config?.enabled_bands?.length ?? 0}
														{#if bandsCount > 0}
															<span class="text-neutral-700">(est. ~{Math.ceil(bandsCount * 50 / 60)}min)</span>
														{/if}
													{:else}
														connection will resume when scan completes
													{/if}
												</div>
											</div>
										{:else if scan?.state === 'running'}
											<div class="text-[10px]">
												<div class="flex items-center gap-1.5">
													<span class="text-cyan-400 animate-pulse">scanning</span>
													<span class="text-neutral-500">{scan.bands_scanned.length}/{scan.bands_to_scan.length} bands</span>
													{#if scan.current_band}
														<span class="text-neutral-400">B{scan.current_band}</span>
													{/if}
												</div>
												<div class="h-1 bg-neutral-800 rounded-full mt-1 overflow-hidden">
													<div
														class="h-full bg-cyan-500/70 rounded-full transition-all duration-500"
														style="width: {scan.bands_to_scan.length > 0 ? (scan.bands_scanned.length / scan.bands_to_scan.length * 100) : 0}%"
													></div>
												</div>
												<div class="text-[9px] text-neutral-600 mt-0.5">
													connection may drop during scan
												</div>
											</div>
										{/if}
									</div>
								{/if}

								<!-- Restore button (after scan completes) -->
								{#if lteData?.scan_status?.state === 'completed' && lteData.scan_status.original_bands.length > 0}
									{@const scan = lteData.scan_status}
									<div class="mt-1.5">
										<button
											class="text-[9px] transition-colors border rounded px-1.5 py-0.5
												{lteConfirm === 'restore'
													? 'text-amber-400 border-amber-600 bg-amber-500/10'
													: 'text-neutral-600 hover:text-neutral-300 border-neutral-800/60'}"
											disabled={lteBandAction !== 'idle'}
											onclick={() => {
												if (lteConfirm === 'restore') { lteConfirm = null; handleRestoreBands(); }
												else { lteConfirm = 'restore'; }
											}}
											onmouseleave={() => { if (lteConfirm === 'restore') lteConfirm = null; }}
										>{lteConfirm === 'restore' ? 'restore?' : 'restore original'}: B{scan.original_bands.join(',B')}{#if scan.original_priority} pri B{scan.original_priority}{/if}</button>
									</div>
								{/if}

								<!-- Watchdog status (inline) -->
								{#if watchdogInfo}
									<div class="border-t border-neutral-800/40 pt-1.5 mt-1.5">
										<div class="flex items-center gap-1.5 text-[10px]">
											<span class="{(watchdogInfo.level ?? 0) === 0 ? 'text-green-500/70' : (watchdogInfo.level ?? 0) <= 1 ? 'text-amber-500/80' : 'text-red-400/80'}">watchdog</span>
											<span class="{(watchdogInfo.level ?? 0) === 0 ? 'text-neutral-600' : 'text-amber-400/80'}">L{watchdogInfo.level ?? 0}</span>
											{#if watchdogInfo.action}
												<span class="text-neutral-600">{watchdogInfo.action}</span>
											{/if}
											{#if watchdogInfo.disconnect_secs != null}
												<span class="text-neutral-700">|</span>
												<span class="text-neutral-600">offline {watchdogInfo.disconnect_secs}s</span>
											{/if}
											{#if watchdogInfo.signal_stale}
												<span class="text-neutral-700">|</span>
												<span class="text-amber-500/60">signal stale</span>
											{/if}
										</div>
									</div>
								{/if}
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
								<span class="text-neutral-400 truncate">{deviceInfo.tunnel.relay_url ?? deviceInfo.tunnel.url ?? ''}</span>
							</div>
						</div>
					{/if}

					<!-- Server process info -->
					{@render serverInfo('server', serverDiagnostics, diagLoading,
						connectionStatus === 'connected' ? () => { diagLoading = true; onfetchdiagnostics?.(); } : undefined)}

				{:else if connectionStatus === 'connected'}
					<div class="flex items-center justify-center h-32 text-[10px] text-neutral-600">
						loading system info...
					</div>
				{:else if offlineSnapshot}
					{@const snap = offlineSnapshot}
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
						<span class="text-[10px] text-neutral-600">[ last known state ]</span>
						<div class="text-[10px] text-neutral-500 tabular-nums space-y-0.5">
							{#if snap.last_lte_signal}
								<div class="flex items-center gap-1.5">
									<span class="text-neutral-600">signal</span>
									{#if snap.last_lte_signal.signal_bars != null}
										<span class="text-neutral-400">{snap.last_lte_signal.signal_bars} bar{snap.last_lte_signal.signal_bars !== 1 ? 's' : ''}</span>
									{/if}
									{#if snap.last_lte_signal.band}
										<span class="text-neutral-600">{snap.last_lte_signal.band}</span>
									{/if}
									{#if snap.last_lte_signal.rssi_dbm != null}
										<span class="text-neutral-700">|</span>
										<span class="text-neutral-600">rssi {snap.last_lte_signal.rssi_dbm}dBm</span>
									{/if}
									{#if snap.last_lte_signal.operator}
										<span class="text-neutral-700">|</span>
										<span class="text-neutral-600">{snap.last_lte_signal.operator}</span>
									{/if}
								</div>
							{/if}
							{#if snap.last_watchdog}
								<div class="flex items-center gap-1.5">
									<span class="text-amber-500/80">watchdog</span>
									<span class="text-amber-400/80">L{snap.last_watchdog.level}</span>
									{#if snap.last_watchdog.action}
										<span class="text-neutral-600">{snap.last_watchdog.action}</span>
									{/if}
									{#if snap.last_watchdog.disconnect_secs != null}
										<span class="text-neutral-700">|</span>
										<span class="text-neutral-600">offline {snap.last_watchdog.disconnect_secs}s</span>
									{/if}
								</div>
							{/if}
							<div class="text-neutral-700">
								last seen {formatTimeAgo(snap.last_seen * 1000, tick)}
							</div>
						</div>
					</div>
				{:else}
					<div class="flex items-center justify-center h-32 text-[10px] text-neutral-700">
						not connected
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
					<!-- Tunnel: multi-device list -->
					<div class="border border-neutral-800/60 rounded p-2.5 space-y-1.5">
						<span class="text-[10px] text-neutral-600">[ tunnel ]</span>
						{#if tunnelDevices.length > 0}
							<div class="space-y-0.5">
								{#each tunnelDevices as dev}
									<button
										class="flex items-center gap-1.5 text-[10px] tabular-nums w-full text-left hover:bg-neutral-800/30 rounded px-1 -mx-1 py-0.5 transition-colors"
										onclick={() => copySerial(dev.serial)}
										title="Copy serial"
									>
										<span class="w-1.5 h-1.5 rounded-full shrink-0 {dev.connected ? 'bg-green-500' : 'bg-neutral-600'}"></span>
										<span class="{dev.isCurrentDevice ? 'text-neutral-200' : 'text-neutral-400'} truncate">{dev.serial}</span>
										{#if dev.connected}
											<span class="text-green-400/70">connected</span>
											<span class="text-neutral-600">{dev.durationLabel}</span>
										{:else}
											<span class="text-neutral-600">offline</span>
											{#if dev.lastSeenLabel}
												<span class="text-neutral-700">last seen {dev.lastSeenLabel}</span>
											{/if}
										{/if}
									</button>
								{/each}
							</div>
						{:else}
							<div class="text-[9px] text-neutral-700">no devices registered</div>
						{/if}
						<!-- Relay-level tunnel stats -->
						{#if relayHealth.tunnel.uptime_secs != null}
							<div class="border-t border-neutral-800/40 pt-1 text-[10px] text-neutral-500 tabular-nums">
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
					</div>

					<!-- Device offline probe (only when device is offline) -->
					{#if !relayDeviceConnected}
						<div class="border border-neutral-800/60 rounded p-2.5 space-y-1">
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
								<div class="flex items-center gap-2">
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
						</div>
					{/if}

					<!-- Relay server process info -->
					{@render serverInfo('relay server', relayDiagnostics, relayDiagLoading,
						hasRelayApiKey ? () => { relayDiagLoading = true; onfetchrelaydiagnostics?.(); } : undefined)}

				{:else}
					<div class="flex items-center justify-center h-32 text-[10px] text-neutral-600">
						loading relay info...
					</div>
				{/if}
			{/if}
		</div>

		<!-- Right column -->
		<div class="w-1/2 min-h-0 overflow-hidden flex flex-col font-mono text-[11px]">
			{#if activeView === 'device'}
				<!-- Device tab bar -->
				<div class="flex items-center gap-1 px-3 py-1.5 border-b border-neutral-800 shrink-0">
					{@render tabButton('activity', deviceRightTab === 'activity', () => { deviceRightTab = 'activity'; })}
					{@render tabButton('telemetry', deviceRightTab === 'telemetry', () => { deviceRightTab = 'telemetry'; }, telemetryLog.length || undefined)}
					{@render tabButton('logs', deviceRightTab === 'logs', () => { deviceRightTab = 'logs'; },
						serverDiagnostics ? serverDiagnostics.logs.length : undefined)}
					{@render tabButton('history', deviceRightTab === 'history', () => { deviceRightTab = 'history'; },
						deviceHistoryTimeline.length ? deviceHistorySummary?.sessionCount : undefined)}
				</div>
				<!-- Device tab content -->
				<div class="flex-1 min-h-0 overflow-hidden flex flex-col">
					{#if deviceRightTab === 'activity'}
						<HistoryViewer entries={activity} {restClient} {onOpenViewer} />
					{:else if deviceRightTab === 'telemetry'}
						{@render tabHeader('Telemetry', `${telemetryLog.length}`)}
						<div class="flex-1 overflow-y-auto min-h-0">
							{@render eventLog(telemetryLog)}
							{#if telemetryLog.length === 0}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									No telemetry recorded yet
								</div>
							{/if}
						</div>
					{:else if deviceRightTab === 'logs'}
						{@const logStats = serverDiagnostics?.log_stats}
						{@render tabHeader('Service Logs',
							logStats ? [
								`${serverDiagnostics?.logs.length ?? 0}`,
								logStats.errors > 0 ? `${logStats.errors} err` : '',
								logStats.warnings > 0 ? `${logStats.warnings} warn` : '',
							].filter(Boolean).join(' / ') : undefined,
							connectionStatus === 'connected' ? () => { diagLoading = true; onfetchdiagnostics?.(); } : undefined,
							diagLoading)}
						<div class="flex-1 overflow-y-auto min-h-0">
							{#if serverDiagnostics && serverDiagnostics.logs.length > 0}
								{@render serviceLogs(serverDiagnostics)}
							{:else if diagLoading}
								<div class="flex items-center justify-center py-8 text-neutral-600 animate-pulse">
									loading...
								</div>
							{:else if connectionStatus !== 'connected'}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									device offline
								</div>
							{:else if serverDiagnostics}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									no log entries
								</div>
							{:else}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									click refresh to load
								</div>
							{/if}
						</div>
					{:else if deviceRightTab === 'history'}
						{@render tabHeader('Connection History',
							deviceHistorySummary ? `${deviceHistorySummary.sessionCount} sessions` : undefined,
							() => { onrefreshrelayhealth?.(); relayCountdown = 15; })}
						<div class="flex-1 overflow-y-auto min-h-0">
							{#if deviceHistoryTimeline.length > 0}
								<div>
									{#each deviceHistoryTimeline as entry}
										{#if entry.kind === 'connected'}
											<div class="flex items-center gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 hover:bg-neutral-800/40 transition-colors">
												<span class="text-neutral-600 w-20 shrink-0 text-right">{entry.time}</span>
												<span class="{entry.active ? 'text-green-400' : 'text-neutral-500'} w-16 shrink-0 text-right">{entry.durationLabel}</span>
												<span class="{entry.active ? 'text-green-400' : 'text-neutral-400'}">connected</span>
												{#if entry.reason}
													<span class="{entry.reasonColor}">{reasonLabel(entry.reason)}</span>
												{/if}
											</div>
										{:else}
											<div class="flex items-center gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 opacity-50">
												<span class="text-neutral-600 w-20 shrink-0 text-right">{entry.time}</span>
												<span class="text-red-400/60 w-16 shrink-0 text-right">{entry.durationLabel}</span>
												<span class="text-red-400/60">offline</span>
											</div>
										{/if}
									{/each}
								</div>
								{#if deviceHistorySummary}
									<div class="px-3 py-1.5 border-t border-neutral-800/40 flex items-center gap-1.5 text-[10px] text-neutral-600 tabular-nums">
										<span>uptime <span class="{deviceHistorySummary.uptimePct > 90 ? 'text-green-400/70' : deviceHistorySummary.uptimePct > 50 ? 'text-amber-400/70' : 'text-red-400/70'}">{deviceHistorySummary.uptimePct}%</span></span>
										<span class="text-neutral-800">&middot;</span>
										<span>{deviceHistorySummary.sessionCount} sessions</span>
										{#if deviceHistorySummary.topReason}
											<span class="text-neutral-800">&middot;</span>
											<span>{deviceHistorySummary.topReasonCount} {reasonLabel(deviceHistorySummary.topReason)}</span>
										{/if}
									</div>
								{/if}
								{#if insights.length > 0}
									<div class="px-3 py-1.5 border-t border-neutral-800/40 space-y-0.5">
										{#each insights as insight}
											<div class="flex gap-1.5 text-[10px]">
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
							{:else}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									{#if !relayHealth}
										no relay data available
									{:else if !relaySerial}
										no device serial configured
									{:else}
										no connection sessions recorded
									{/if}
								</div>
							{/if}
						</div>
					{/if}
				</div>
			{:else}
				<!-- Relay tab bar -->
				<div class="flex items-center gap-1 px-3 py-1.5 border-b border-neutral-800 shrink-0">
					{@render tabButton('connection', relayRightTab === 'connection', () => { relayRightTab = 'connection'; }, connectionLog.length || undefined)}
					{@render tabButton('logs', relayRightTab === 'logs', () => { relayRightTab = 'logs'; },
						relayDiagnostics ? relayDiagnostics.logs.length : undefined)}
					{@render tabButton('history', relayRightTab === 'history', () => { relayRightTab = 'history'; },
						relayHistorySummary?.sessionCount)}
				</div>
				<!-- Relay tab content -->
				<div class="flex-1 min-h-0 overflow-hidden flex flex-col">
					{#if relayRightTab === 'connection'}
						{@render tabHeader('Connection Log', `${connectionLog.length}`)}
						<div class="flex-1 overflow-y-auto min-h-0">
							{@render eventLog(connectionLog)}
							{#if connectionLog.length === 0}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									No connection events yet
								</div>
							{/if}
						</div>
					{:else if relayRightTab === 'logs'}
						{@const logStats = relayDiagnostics?.log_stats}
						{@render tabHeader('Relay Service Logs',
							logStats ? [
								`${relayDiagnostics?.logs.length ?? 0}`,
								logStats.errors > 0 ? `${logStats.errors} err` : '',
								logStats.warnings > 0 ? `${logStats.warnings} warn` : '',
							].filter(Boolean).join(' / ') : undefined,
							hasRelayApiKey ? () => { relayDiagLoading = true; onfetchrelaydiagnostics?.(); } : undefined,
							relayDiagLoading)}
						<div class="flex-1 overflow-y-auto min-h-0">
							{#if relayDiagnostics && relayDiagnostics.logs.length > 0}
								{@render serviceLogs(relayDiagnostics)}
							{:else if relayDiagLoading}
								<div class="flex items-center justify-center py-8 text-neutral-600 animate-pulse">
									loading...
								</div>
							{:else if !hasRelayApiKey}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									add relay api key to view logs
								</div>
							{:else if relayDiagnostics}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									no log entries
								</div>
							{:else}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									click refresh to load
								</div>
							{/if}
						</div>
					{:else if relayRightTab === 'history'}
						{@render tabHeader('Connection History',
							relayHistorySummary ? `${relayHistorySummary.sessionCount} sessions` : undefined,
							() => { onrefreshrelayhealth?.(); relayCountdown = 15; })}
						<div class="flex-1 overflow-y-auto min-h-0">
							{#if relayHistoryTimeline.length > 0}
								<div>
									{#each relayHistoryTimeline as entry}
										{#if entry.kind === 'connected'}
											<div class="flex items-center gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 hover:bg-neutral-800/40 transition-colors">
												<span class="text-neutral-600 w-20 shrink-0 text-right">{entry.time}</span>
												<span class="{entry.active ? 'text-green-400' : 'text-neutral-500'} w-16 shrink-0 text-right">{entry.durationLabel}</span>
												{#if entry.serial}
													<span class="{entry.serial === relaySerial ? 'text-neutral-300' : 'text-neutral-500'} w-28 shrink-0 truncate" title={entry.serial}>{entry.serial}</span>
												{/if}
												<span class="{entry.active ? 'text-green-400' : 'text-neutral-400'}">connected</span>
												{#if entry.reason}
													<span class="{entry.reasonColor}">{reasonLabel(entry.reason)}</span>
												{/if}
											</div>
										{:else}
											<div class="flex items-center gap-1.5 py-1 px-3 text-[10px] tabular-nums border-b border-neutral-800/20 opacity-50">
												<span class="text-neutral-600 w-20 shrink-0 text-right">{entry.time}</span>
												<span class="text-red-400/60 w-16 shrink-0 text-right">{entry.durationLabel}</span>
												<span class="text-red-400/60">offline</span>
											</div>
										{/if}
									{/each}
								</div>
								{#if relayHistorySummary}
									<div class="px-3 py-1.5 border-t border-neutral-800/40 flex items-center gap-1.5 text-[10px] text-neutral-600 tabular-nums">
										<span>uptime <span class="{relayHistorySummary.uptimePct > 90 ? 'text-green-400/70' : relayHistorySummary.uptimePct > 50 ? 'text-amber-400/70' : 'text-red-400/70'}">{relayHistorySummary.uptimePct}%</span></span>
										<span class="text-neutral-800">&middot;</span>
										<span>{relayHistorySummary.sessionCount} sessions</span>
										{#if relayHistorySummary.topReason}
											<span class="text-neutral-800">&middot;</span>
											<span>{relayHistorySummary.topReasonCount} {reasonLabel(relayHistorySummary.topReason)}</span>
										{/if}
									</div>
								{/if}
								{#if insights.length > 0}
									<div class="px-3 py-1.5 border-t border-neutral-800/40 space-y-0.5">
										{#each insights as insight}
											<div class="flex gap-1.5 text-[10px]">
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
							{:else}
								<div class="flex items-center justify-center py-8 text-neutral-600">
									no connection sessions recorded
								</div>
							{/if}
						</div>
					{/if}
				</div>
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
				<span class="text-neutral-700 ml-1">{relayDeviceConnected ? '' : '(no device)'}</span>
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
