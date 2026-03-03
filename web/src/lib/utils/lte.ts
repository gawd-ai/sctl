import type { BandHistoryEntry, ScanBandResult } from '../types/terminal.types';

// 3GPP TS 36.101 EARFCN → frequency lookup
// F_DL = F_DL_low + 0.1 * (EARFCN - N_offs_DL)

interface BandEntry {
	band: number;
	dlLow: number; // MHz
	dlOffset: number; // N_offs_DL
	dlHigh: number; // max EARFCN for this band
}

// NA + common global bands
const BANDS: BandEntry[] = [
	{ band: 1, dlLow: 2110, dlOffset: 0, dlHigh: 599 },
	{ band: 2, dlLow: 1930, dlOffset: 600, dlHigh: 1199 },
	{ band: 3, dlLow: 1805, dlOffset: 1200, dlHigh: 1949 },
	{ band: 4, dlLow: 2110, dlOffset: 1950, dlHigh: 2399 },
	{ band: 5, dlLow: 869, dlOffset: 2400, dlHigh: 2649 },
	{ band: 7, dlLow: 2620, dlOffset: 2750, dlHigh: 3449 },
	{ band: 8, dlLow: 925, dlOffset: 3450, dlHigh: 3799 },
	{ band: 12, dlLow: 729, dlOffset: 5010, dlHigh: 5179 },
	{ band: 13, dlLow: 746, dlOffset: 5180, dlHigh: 5279 },
	{ band: 14, dlLow: 758, dlOffset: 5280, dlHigh: 5379 },
	{ band: 17, dlLow: 734, dlOffset: 5730, dlHigh: 5849 },
	{ band: 20, dlLow: 791, dlOffset: 6150, dlHigh: 6449 },
	{ band: 25, dlLow: 1930, dlOffset: 8040, dlHigh: 8689 },
	{ band: 26, dlLow: 859, dlOffset: 8690, dlHigh: 9039 },
	{ band: 28, dlLow: 758, dlOffset: 9210, dlHigh: 9659 },
	{ band: 29, dlLow: 717, dlOffset: 9660, dlHigh: 9769 },
	{ band: 30, dlLow: 2350, dlOffset: 9770, dlHigh: 9869 },
	{ band: 66, dlLow: 2110, dlOffset: 66436, dlHigh: 67335 },
	{ band: 71, dlLow: 617, dlOffset: 68586, dlHigh: 68935 },
];

export interface EarfcnResult {
	band: number;
	frequencyMhz: number;
}

/** Look up band and DL frequency for an EARFCN. Returns null if not in table. */
export function earfcnInfo(earfcn: number): EarfcnResult | null {
	for (const b of BANDS) {
		if (earfcn >= b.dlOffset && earfcn <= b.dlHigh) {
			return {
				band: b.band,
				frequencyMhz: Math.round((b.dlLow + 0.1 * (earfcn - b.dlOffset)) * 10) / 10,
			};
		}
	}
	return null;
}

/** Get the DL start frequency for a band number. Returns null if unknown. */
export function bandFrequencyMhz(band: number): number | null {
	const entry = BANDS.find((b) => b.band === band);
	return entry?.dlLow ?? null;
}

export interface BandOverviewEntry {
	band: number;
	frequencyMhz: number | null;
	enabled: boolean;
	active: boolean;
	priority: boolean;
	rsrp: number | null;
	cellCount: number;
}

/**
 * Aggregate per-band signal overview from serving cell + neighbor data.
 * Sorted: active band first, then by band number.
 */
export function bandOverview(params: {
	enabledBands: number[];
	priorityBand: number | undefined | null;
	servingBand: number | undefined | null;
	servingRsrp: number | undefined | null;
	neighbors: { earfcn: number; rsrp?: number | null }[];
}): BandOverviewEntry[] {
	const { enabledBands, priorityBand, servingBand, servingRsrp, neighbors } = params;
	const map = new Map<number, BandOverviewEntry>();

	// Initialize from enabled bands
	for (const b of enabledBands) {
		map.set(b, {
			band: b,
			frequencyMhz: bandFrequencyMhz(b),
			enabled: true,
			active: servingBand === b,
			priority: priorityBand === b,
			rsrp: servingBand === b && servingRsrp != null ? servingRsrp : null,
			cellCount: 0,
		});
	}

	// Aggregate neighbor cells
	for (const n of neighbors) {
		const info = earfcnInfo(n.earfcn);
		if (!info) continue;
		const b = info.band;
		let entry = map.get(b);
		if (!entry) {
			// Neighbor on a band not in enabled list
			entry = {
				band: b,
				frequencyMhz: info.frequencyMhz,
				enabled: false,
				active: false,
				priority: false,
				rsrp: null,
				cellCount: 0,
			};
			map.set(b, entry);
		}
		entry.cellCount++;
		if (n.rsrp != null && !entry.active) {
			// Track best neighbor RSRP (don't overwrite serving cell RSRP)
			if (entry.rsrp == null || n.rsrp > entry.rsrp) {
				entry.rsrp = n.rsrp;
			}
		}
	}

	// Sort: active first, then by band number
	return [...map.values()].sort((a, b) => {
		if (a.active !== b.active) return a.active ? -1 : 1;
		return a.band - b.band;
	});
}

/** Tailwind text color class for an RSRP value. */
export function rsrpColor(rsrp: number): string {
	if (rsrp >= -80) return 'text-green-400';
	if (rsrp >= -100) return 'text-neutral-400';
	if (rsrp >= -110) return 'text-amber-400';
	return 'text-red-400';
}

/** Tailwind background color class for a signal bars value (0–5). */
export function signalBgColor(bars: number): string {
	if (bars >= 4) return 'bg-green-400';
	if (bars >= 2) return 'bg-amber-400';
	return 'bg-red-400';
}

// ── Unified band overview ──────────────────────────────────────────

export type BandDataSource = 'serving' | 'scan' | 'history' | 'neighbor';

export interface UnifiedBandEntry {
	band: number;
	frequencyMhz: number | null;
	serving: boolean;
	enabled: boolean;
	priority: boolean;
	lockable: boolean;
	rsrp: number | null;
	rsrpSource: BandDataSource | null;
	cellCount: number;
	scan: {
		registered: boolean;
		rsrp: number | null;
		sinr: number | null;
		downloadBps: number | null;
		registrationTimeMs: number;
	} | null;
	history: {
		bestRsrp: number;
		latestRsrp: number;
		observationCount: number;
		lastSeen: number;
		trend: 'up' | 'down' | 'stable' | null;
	} | null;
	sources: BandDataSource[];
}

/** Compute RSRP trend from recent observations. Needs 4+ to be meaningful. */
function computeTrend(recent: { rsrp: number }[]): 'up' | 'down' | 'stable' | null {
	if (recent.length < 4) return null;
	const mid = Math.floor(recent.length / 2);
	const firstHalf = recent.slice(0, mid);
	const secondHalf = recent.slice(mid);
	const avg = (arr: { rsrp: number }[]) => arr.reduce((s, o) => s + o.rsrp, 0) / arr.length;
	const diff = avg(secondHalf) - avg(firstHalf);
	if (diff > 2) return 'up';
	if (diff < -2) return 'down';
	return 'stable';
}

/** Merge all band data sources into a single sorted list. */
export function unifiedBandOverview(params: {
	enabledBands: number[];
	priorityBand: number | undefined | null;
	servingBand: number | undefined | null;
	servingRsrp: number | undefined | null;
	neighbors: { earfcn: number; rsrp?: number | null }[];
	bandHistory: BandHistoryEntry[];
	scanResults: ScanBandResult[];
}): UnifiedBandEntry[] {
	const { enabledBands, priorityBand, servingBand, servingRsrp, neighbors, bandHistory, scanResults } = params;
	const map = new Map<number, UnifiedBandEntry>();

	function getOrCreate(band: number): UnifiedBandEntry {
		let entry = map.get(band);
		if (!entry) {
			entry = {
				band,
				frequencyMhz: bandFrequencyMhz(band),
				serving: false,
				enabled: false,
				priority: false,
				lockable: true,
				rsrp: null,
				rsrpSource: null,
				cellCount: 0,
				scan: null,
				history: null,
				sources: [],
			};
			map.set(band, entry);
		}
		return entry;
	}

	// 1. Seed from enabled bands
	for (const b of enabledBands) {
		const entry = getOrCreate(b);
		entry.enabled = true;
		entry.lockable = true;
		if (b === servingBand) {
			entry.serving = true;
			if (servingRsrp != null) {
				entry.rsrp = servingRsrp;
				entry.rsrpSource = 'serving';
			}
			if (!entry.sources.includes('serving')) entry.sources.push('serving');
		}
		if (b === priorityBand) entry.priority = true;
	}

	// 2. Merge neighbors
	for (const n of neighbors) {
		const info = earfcnInfo(n.earfcn);
		if (!info) continue;
		const entry = getOrCreate(info.band);
		if (!entry.frequencyMhz) entry.frequencyMhz = info.frequencyMhz;
		entry.cellCount++;
		if (n.rsrp != null && !entry.serving) {
			if (entry.rsrp == null || (entry.rsrpSource === 'neighbor' && n.rsrp > entry.rsrp)) {
				entry.rsrp = n.rsrp;
				entry.rsrpSource = 'neighbor';
			}
		}
		if (!entry.sources.includes('neighbor')) entry.sources.push('neighbor');
	}

	// 3. Merge band history
	for (const h of bandHistory) {
		const entry = getOrCreate(h.band);
		entry.history = {
			bestRsrp: h.best_rsrp,
			latestRsrp: h.latest_rsrp,
			observationCount: h.observation_count,
			lastSeen: h.last_seen,
			trend: computeTrend(h.recent),
		};
		// History RSRP outranks neighbor but not serving
		if (entry.rsrp == null || entry.rsrpSource === 'neighbor') {
			entry.rsrp = h.latest_rsrp;
			entry.rsrpSource = 'history';
		}
		if (!entry.sources.includes('history')) entry.sources.push('history');
	}

	// 4. Merge scan results
	for (const r of scanResults) {
		const entry = getOrCreate(r.band);
		entry.scan = {
			registered: r.registered,
			rsrp: r.rsrp ?? null,
			sinr: r.sinr ?? null,
			downloadBps: r.download_bps ?? null,
			registrationTimeMs: r.registration_time_ms,
		};
		// For new bands from scan: lockable only if registered
		if (!entry.enabled && !entry.sources.includes('neighbor') && !entry.sources.includes('history')) {
			entry.lockable = r.registered;
		}
		// Scan RSRP outranks history/neighbor but not serving
		if (r.rsrp != null && !entry.serving) {
			if (entry.rsrp == null || entry.rsrpSource === 'neighbor' || entry.rsrpSource === 'history') {
				entry.rsrp = r.rsrp;
				entry.rsrpSource = 'scan';
			}
		}
		if (!entry.sources.includes('scan')) entry.sources.push('scan');
	}

	// Sort: serving first, then enabled (by band#), then lockable non-enabled (by RSRP desc), then non-lockable
	return [...map.values()].sort((a, b) => {
		if (a.serving !== b.serving) return a.serving ? -1 : 1;
		if (a.enabled !== b.enabled) return a.enabled ? -1 : 1;
		if (a.enabled && b.enabled) return a.band - b.band;
		if (a.lockable !== b.lockable) return a.lockable ? -1 : 1;
		// Both lockable non-enabled: sort by RSRP descending, nulls last
		if (a.lockable && b.lockable) {
			if (a.rsrp != null && b.rsrp != null) return b.rsrp - a.rsrp;
			if (a.rsrp != null) return -1;
			if (b.rsrp != null) return 1;
		}
		return a.band - b.band;
	});
}
