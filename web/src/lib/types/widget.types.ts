/**
 * Simplified connection config for widgets. Widgets create their own
 * WS/REST clients internally — pass this instead of a full `SctlinConfig`.
 */
export interface DeviceConnectionConfig {
	/** WebSocket URL for the sctl device (e.g. `'ws://host:1337/api/ws'`). */
	wsUrl: string;
	/** API key for authentication (Bearer token). */
	apiKey: string;
	/** Connect automatically on mount. Default: true. */
	autoConnect?: boolean;
}
