/**
 * Typed error hierarchy for sctl client operations.
 *
 * All errors extend `SctlError`, which carries a machine-readable `code` field
 * for programmatic handling alongside the human-readable `message`.
 *
 * @example
 * ```ts
 * try {
 *   await ws.startSession();
 * } catch (e) {
 *   if (e instanceof TimeoutError) console.log('timed out');
 *   if (e instanceof SctlError) console.log(e.code, e.message);
 * }
 * ```
 */

/** Base error class for all sctl client errors. */
export class SctlError extends Error {
	/** Machine-readable error code (e.g. `'timeout'`, `'connection_error'`). */
	readonly code: string;

	constructor(code: string, message: string) {
		super(message);
		this.name = 'SctlError';
		this.code = code;
	}
}

/** Thrown when a WebSocket operation fails due to connection state. */
export class ConnectionError extends SctlError {
	constructor(message: string) {
		super('connection_error', message);
		this.name = 'ConnectionError';
	}
}

/**
 * Thrown when the server responds with an error message (WsErrorMsg).
 * The `code` field is the server-provided error code.
 */
export class ServerError extends SctlError {
	constructor(code: string, message: string) {
		super(code, message);
		this.name = 'ServerError';
	}
}

/** Thrown when an operation exceeds its timeout (ack timeout, HTTP timeout). */
export class TimeoutError extends SctlError {
	constructor(message: string) {
		super('timeout', message);
		this.name = 'TimeoutError';
	}
}

/** Thrown when an HTTP request returns a non-OK status code. */
export class HttpError extends SctlError {
	/** HTTP status code (e.g. 404, 500). */
	readonly status: number;
	/** Response body text. */
	readonly body: string;

	constructor(status: number, body: string) {
		super('http_error', `${status}: ${body}`);
		this.name = 'HttpError';
		this.status = status;
		this.body = body;
	}
}

/** Thrown when a file transfer fails (hash mismatch, chunk rejected, etc.). */
export class TransferError extends SctlError {
	/** The transfer ID that failed, if known. */
	readonly transferId?: string;

	constructor(message: string, transferId?: string) {
		super('transfer_error', message);
		this.name = 'TransferError';
		this.transferId = transferId;
	}
}
