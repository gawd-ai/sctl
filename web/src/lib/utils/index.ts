export { SctlWsClient } from './ws-client';
export type { WsClientConfig } from './ws-client';
export { createTerminal, applyTheme, DEFAULT_THEME, type XtermInstance } from './xterm';
export { SctlRestClient } from './rest-client';
export type { RestClientConfig } from './rest-client';
export { KeyboardManager, type Shortcut } from './keyboard';
export { parsePlaybookFrontmatter, renderPlaybookScript, validatePlaybookName } from './playbook-parser';
export { TransferTracker, type ClientTransfer, type TransferProgress, type TransferState, type OnProgress, type OnComplete, type OnError } from './transfer';
export { ConnectionManager, type ConnectionManagerConfig, type ConnectionManagerEvents, type ServerConnection } from './connection-manager';
export { SctlError, ConnectionError, ServerError, TimeoutError, HttpError, TransferError } from './errors';
export { earfcnInfo, rsrpColor, bandFrequencyMhz, bandOverview, unifiedBandOverview } from './lte';
export type { EarfcnResult, BandOverviewEntry, UnifiedBandEntry, BandDataSource } from './lte';

/** Generate a UUID v4, with fallback for non-secure contexts (plain HTTP). */
export function uuid(): string {
	if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
		return crypto.randomUUID();
	}
	// Fallback: crypto.getRandomValues is available in all modern browsers regardless of secure context
	const bytes = new Uint8Array(16);
	crypto.getRandomValues(bytes);
	bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
	bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
	const hex = [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
