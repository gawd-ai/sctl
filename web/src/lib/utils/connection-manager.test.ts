import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { ConnectionManager } from './connection-manager';
import type { ServerConfig } from '../types/terminal.types';

// ── Mocks ─────────────────────────────────────────────────────────

const mockConnect = vi.fn();
const mockDisconnect = vi.fn();
const mockOnStatusChange = vi.fn().mockReturnValue(() => {});

vi.mock('./ws-client', () => {
	return {
		SctlWsClient: class MockSctlWsClient {
			connect = mockConnect;
			disconnect = mockDisconnect;
			onStatusChange = mockOnStatusChange;
			status = 'disconnected';
			wsUrl = '';
			apiKey = '';
			constructor() {}
		}
	};
});

const mockGetInfo = vi.fn();
const mockGetActivity = vi.fn();

vi.mock('./rest-client', () => {
	return {
		SctlRestClient: class MockSctlRestClient {
			getInfo = mockGetInfo;
			getActivity = mockGetActivity;
			constructor() {}
		}
	};
});

vi.mock('./transfer', () => {
	return {
		TransferTracker: class MockTransferTracker {
			activeTransfers: never[] = [];
			onchange: (() => void) | null = null;
			onerror: (() => void) | null = null;
			constructor() {}
		}
	};
});

const SERVER: ServerConfig = {
	id: 'test-1',
	name: 'Test Server',
	wsUrl: 'ws://localhost:1337/api/ws',
	apiKey: 'test-key',
	shell: ''
};

const SERVER2: ServerConfig = {
	id: 'test-2',
	name: 'Test Server 2',
	wsUrl: 'ws://localhost:1338/api/ws',
	apiKey: 'test-key-2',
	shell: '/bin/zsh'
};

describe('ConnectionManager', () => {
	let manager: ConnectionManager;

	beforeEach(() => {
		vi.clearAllMocks();
		manager = new ConnectionManager();
	});

	afterEach(() => {
		manager.destroy();
	});

	describe('connect', () => {
		it('creates a connection and calls wsClient.connect()', () => {
			const conn = manager.connect(SERVER);
			expect(conn.id).toBe('test-1');
			expect(conn.config).toBe(SERVER);
			expect(conn.deviceInfo).toBeNull();
			expect(conn.activity).toEqual([]);
			expect(mockConnect).toHaveBeenCalledOnce();
		});

		it('returns existing connection on duplicate connect', () => {
			const conn1 = manager.connect(SERVER);
			const conn2 = manager.connect(SERVER);
			expect(conn1).toBe(conn2);
			expect(mockConnect).toHaveBeenCalledOnce();
		});

		it('registers a status change listener', () => {
			manager.connect(SERVER);
			expect(mockOnStatusChange).toHaveBeenCalledOnce();
		});
	});

	describe('disconnect', () => {
		it('calls wsClient.disconnect() and removes the connection', () => {
			manager.connect(SERVER);
			expect(manager.get('test-1')).toBeDefined();

			manager.disconnect('test-1');
			expect(mockDisconnect).toHaveBeenCalledOnce();
			expect(manager.get('test-1')).toBeUndefined();
		});

		it('no-op for unknown server', () => {
			manager.disconnect('nonexistent');
			expect(mockDisconnect).not.toHaveBeenCalled();
		});
	});

	describe('get / getAll', () => {
		it('returns undefined for unknown server', () => {
			expect(manager.get('unknown')).toBeUndefined();
		});

		it('getAll returns all connections', () => {
			manager.connect(SERVER);
			manager.connect(SERVER2);
			expect(manager.getAll()).toHaveLength(2);
		});
	});

	describe('disconnectAll / destroy', () => {
		it('disconnects all servers', () => {
			manager.connect(SERVER);
			manager.connect(SERVER2);
			manager.disconnectAll();
			expect(manager.getAll()).toHaveLength(0);
			expect(mockDisconnect).toHaveBeenCalledTimes(2);
		});
	});

	describe('fetchDeviceInfo', () => {
		it('fetches and stores device info', async () => {
			const info = { serial: 'abc', hostname: 'test' };
			mockGetInfo.mockResolvedValueOnce(info);

			manager.connect(SERVER);
			const result = await manager.fetchDeviceInfo('test-1');

			expect(result).toEqual(info);
			expect(manager.get('test-1')?.deviceInfo).toEqual(info);
		});

		it('returns null and stores null on error', async () => {
			mockGetInfo.mockRejectedValueOnce(new Error('Network error'));

			manager.connect(SERVER);
			const result = await manager.fetchDeviceInfo('test-1');

			expect(result).toBeNull();
			expect(manager.get('test-1')?.deviceInfo).toBeNull();
		});

		it('returns null for unknown server', async () => {
			const result = await manager.fetchDeviceInfo('unknown');
			expect(result).toBeNull();
		});
	});

	describe('fetchActivity', () => {
		it('fetches and stores activity entries', async () => {
			const entries = [{ id: 1, activity_type: 'exec', source: 'mcp', summary: 'test', timestamp: 123 }];
			mockGetActivity.mockResolvedValueOnce(entries);

			manager.connect(SERVER);
			const result = await manager.fetchActivity('test-1');

			expect(result).toEqual(entries);
			expect(manager.get('test-1')?.activity).toEqual(entries);
		});

		it('returns empty array on error', async () => {
			mockGetActivity.mockRejectedValueOnce(new Error('fail'));

			manager.connect(SERVER);
			const result = await manager.fetchActivity('test-1');

			expect(result).toEqual([]);
		});

		it('returns empty array for unknown server', async () => {
			const result = await manager.fetchActivity('unknown');
			expect(result).toEqual([]);
		});
	});

	describe('events', () => {
		it('fires onDeviceInfo callback', async () => {
			const info = { serial: 'abc', hostname: 'test' };
			mockGetInfo.mockResolvedValueOnce(info);

			const onDeviceInfo = vi.fn();
			const m = new ConnectionManager({}, { onDeviceInfo });
			m.connect(SERVER);
			await m.fetchDeviceInfo('test-1');

			expect(onDeviceInfo).toHaveBeenCalledWith('test-1', info);
			m.destroy();
		});

		it('fires onActivity callback', async () => {
			const entries = [{ id: 1, activity_type: 'exec', source: 'mcp', summary: 'test', timestamp: 123 }];
			mockGetActivity.mockResolvedValueOnce(entries);

			const onActivity = vi.fn();
			const m = new ConnectionManager({}, { onActivity });
			m.connect(SERVER);
			await m.fetchActivity('test-1');

			expect(onActivity).toHaveBeenCalledWith('test-1', entries);
			m.destroy();
		});

		it('fires onError callback on fetch failure', async () => {
			mockGetInfo.mockRejectedValueOnce(new Error('Connection refused'));

			const onError = vi.fn();
			const m = new ConnectionManager({}, { onError });
			m.connect(SERVER);
			await m.fetchDeviceInfo('test-1');

			expect(onError).toHaveBeenCalledWith('test-1', expect.any(Error));
			m.destroy();
		});
	});

	describe('buildSctlinConfig', () => {
		it('returns a valid SctlinConfig with pre-created client', () => {
			const conn = manager.connect(SERVER);
			const cfg = manager.buildSctlinConfig(SERVER);

			expect(cfg.wsUrl).toBe(SERVER.wsUrl);
			expect(cfg.apiKey).toBe(SERVER.apiKey);
			expect(cfg.autoConnect).toBe(true);
			expect(cfg.autoStartSession).toBe(false);
			expect(cfg.client).toBe(conn.wsClient);
			expect(cfg.sessionDefaults?.pty).toBe(true);
			expect(cfg.sessionDefaults?.persistent).toBe(true);
			expect(cfg.callbacks).toBeDefined();
		});

		it('merges consumer callbacks', () => {
			manager.connect(SERVER);
			const onSessionsChange = vi.fn();
			const cfg = manager.buildSctlinConfig(SERVER, { onSessionsChange });

			// Trigger the callback
			cfg.callbacks?.onSessionsChange?.([]);
			expect(onSessionsChange).toHaveBeenCalledWith([]);
		});

		it('applies server shell to sessionDefaults', () => {
			manager.connect(SERVER2);
			const cfg = manager.buildSctlinConfig(SERVER2);

			expect(cfg.sessionDefaults?.shell).toBe('/bin/zsh');
		});
	});

	describe('config options', () => {
		it('accepts custom maxActivityEntries', () => {
			const m = new ConnectionManager({ maxActivityEntries: 50 });
			m.connect(SERVER);
			m.destroy();
		});

		it('accepts autoFetchInfo: false', () => {
			const m = new ConnectionManager({ autoFetchInfo: false, autoFetchActivity: false });
			m.connect(SERVER);
			m.destroy();
		});
	});
});
