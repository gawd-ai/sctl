/** If wsUrl is a relay URL (ws://host/d/{serial}/api/ws), return the relay HTTP base URL. */
export function getRelayBaseUrl(wsUrl: string): string | null {
	const match = wsUrl.match(/^(wss?:\/\/[^/]+)\/d\/[^/]+\/api\/ws/);
	if (!match) return null;
	return match[1].replace(/^wss:/, 'https:').replace(/^ws:/, 'http:');
}

/** Extract the device serial from a relay URL. Returns null for non-relay URLs. */
export function getRelaySerial(wsUrl: string): string | null {
	const match = wsUrl.match(/\/d\/([^/]+)\/api\/ws/);
	return match ? match[1] : null;
}
