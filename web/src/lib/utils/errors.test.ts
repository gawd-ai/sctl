import { describe, it, expect } from 'vitest';
import {
	SctlError,
	ConnectionError,
	ServerError,
	TimeoutError,
	HttpError,
	TransferError
} from './errors';

describe('SctlError', () => {
	it('sets code and message', () => {
		const err = new SctlError('test_code', 'test message');
		expect(err.code).toBe('test_code');
		expect(err.message).toBe('test message');
		expect(err.name).toBe('SctlError');
		expect(err).toBeInstanceOf(Error);
		expect(err).toBeInstanceOf(SctlError);
	});
});

describe('ConnectionError', () => {
	it('has connection_error code', () => {
		const err = new ConnectionError('WebSocket not connected');
		expect(err.code).toBe('connection_error');
		expect(err.message).toBe('WebSocket not connected');
		expect(err.name).toBe('ConnectionError');
		expect(err).toBeInstanceOf(SctlError);
		expect(err).toBeInstanceOf(ConnectionError);
		expect(err).toBeInstanceOf(Error);
	});
});

describe('ServerError', () => {
	it('carries server-provided code', () => {
		const err = new ServerError('session_not_found', 'Session xyz not found');
		expect(err.code).toBe('session_not_found');
		expect(err.message).toBe('Session xyz not found');
		expect(err.name).toBe('ServerError');
		expect(err).toBeInstanceOf(SctlError);
	});
});

describe('TimeoutError', () => {
	it('has timeout code', () => {
		const err = new TimeoutError('Ack timeout for session.start');
		expect(err.code).toBe('timeout');
		expect(err.message).toBe('Ack timeout for session.start');
		expect(err.name).toBe('TimeoutError');
		expect(err).toBeInstanceOf(SctlError);
	});
});

describe('HttpError', () => {
	it('formats status and body', () => {
		const err = new HttpError(404, 'Not found');
		expect(err.code).toBe('http_error');
		expect(err.status).toBe(404);
		expect(err.body).toBe('Not found');
		expect(err.message).toBe('404: Not found');
		expect(err.name).toBe('HttpError');
		expect(err).toBeInstanceOf(SctlError);
	});

	it('handles 500 errors', () => {
		const err = new HttpError(500, 'Internal server error');
		expect(err.status).toBe(500);
		expect(err.message).toBe('500: Internal server error');
	});
});

describe('TransferError', () => {
	it('has transfer_error code', () => {
		const err = new TransferError('Hash mismatch');
		expect(err.code).toBe('transfer_error');
		expect(err.message).toBe('Hash mismatch');
		expect(err.name).toBe('TransferError');
		expect(err.transferId).toBeUndefined();
		expect(err).toBeInstanceOf(SctlError);
	});

	it('carries optional transferId', () => {
		const err = new TransferError('Chunk rejected', 'tx-123');
		expect(err.transferId).toBe('tx-123');
	});
});

describe('instanceof chains', () => {
	it('all errors are instanceof Error', () => {
		const errors = [
			new SctlError('a', 'b'),
			new ConnectionError('c'),
			new ServerError('d', 'e'),
			new TimeoutError('f'),
			new HttpError(400, 'g'),
			new TransferError('h')
		];
		for (const err of errors) {
			expect(err).toBeInstanceOf(Error);
			expect(err).toBeInstanceOf(SctlError);
		}
	});

	it('subclasses are not instanceof each other', () => {
		const timeout = new TimeoutError('t');
		const connection = new ConnectionError('c');
		expect(timeout).not.toBeInstanceOf(ConnectionError);
		expect(connection).not.toBeInstanceOf(TimeoutError);
	});
});
