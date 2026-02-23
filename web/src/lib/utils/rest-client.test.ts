import { describe, it, expect, vi, beforeEach } from 'vitest';
import { SctlRestClient } from './rest-client';

// Mock global fetch
const mockFetch = vi.fn();
vi.stubGlobal('fetch', mockFetch);

describe('SctlRestClient', () => {
	let client: SctlRestClient;

	beforeEach(() => {
		mockFetch.mockReset();
		client = new SctlRestClient('ws://localhost:1337/api/ws', 'test-key');
	});

	function mockJsonResponse(data: unknown, status = 200) {
		mockFetch.mockResolvedValueOnce({
			ok: status >= 200 && status < 300,
			status,
			statusText: 'OK',
			json: () => Promise.resolve(data),
			text: () => Promise.resolve(JSON.stringify(data))
		});
	}

	it('derives HTTP URL from WS URL', () => {
		// The client internally converts ws:// to http:// and strips /api/ws
		mockJsonResponse({ status: 'ok' });
		client.getHealth();
		expect(mockFetch).toHaveBeenCalledWith(
			'http://localhost:1337/api/health',
			expect.objectContaining({
				headers: expect.objectContaining({
					Authorization: 'Bearer test-key'
				})
			})
		);
	});

	it('handles wss:// URLs', () => {
		const secureClient = new SctlRestClient('wss://device.example.com/api/ws', 'key');
		mockJsonResponse({ status: 'ok' });
		secureClient.getHealth();
		expect(mockFetch).toHaveBeenCalledWith(
			'https://device.example.com/api/health',
			expect.any(Object)
		);
	});

	describe('getInfo', () => {
		it('returns device info', async () => {
			const info = { hostname: 'test', cpu: { model: 'arm' } };
			mockJsonResponse(info);
			const result = await client.getInfo();
			expect(result).toEqual(info);
		});
	});

	describe('getHealth', () => {
		it('returns health data', async () => {
			const health = { status: 'ok', uptime: 1234, version: '0.4.0' };
			mockJsonResponse(health);
			const result = await client.getHealth();
			expect(result).toEqual(health);
		});
	});

	describe('exec', () => {
		it('sends POST with command', async () => {
			mockJsonResponse({ exit_code: 0, stdout: 'hello', stderr: '', duration_ms: 10 });
			const result = await client.exec('echo hello');
			expect(mockFetch).toHaveBeenCalledWith(
				'http://localhost:1337/api/exec',
				expect.objectContaining({
					method: 'POST',
					body: expect.stringContaining('"command":"echo hello"')
				})
			);
			expect(result.exit_code).toBe(0);
		});
	});

	describe('getActivity', () => {
		it('returns activity entries', async () => {
			const entries = [{ id: 1, activity_type: 'exec', source: 'mcp', summary: 'test', timestamp: Date.now() }];
			mockJsonResponse({ entries });
			const result = await client.getActivity(0, 50);
			expect(result).toEqual(entries);
		});

		it('returns empty array when no entries key', async () => {
			mockJsonResponse({});
			const result = await client.getActivity();
			expect(result).toEqual([]);
		});
	});

	describe('playbooks', () => {
		it('listPlaybooks returns playbook summaries', async () => {
			const playbooks = [{ name: 'test', description: 'A test', params: ['radio'] }];
			mockJsonResponse({ playbooks });
			const result = await client.listPlaybooks();
			expect(result).toEqual(playbooks);
		});

		it('getPlaybook returns detail', async () => {
			const detail = { name: 'test', description: 'A test', params: {}, script: 'echo hi', raw_content: '...' };
			mockJsonResponse(detail);
			const result = await client.getPlaybook('test');
			expect(result).toEqual(detail);
		});

		it('putPlaybook sends PUT with markdown content', async () => {
			mockJsonResponse({ ok: true });
			await client.putPlaybook('test', '---\nname: test\n---\n```sh\necho hi\n```');
			expect(mockFetch).toHaveBeenCalledWith(
				'http://localhost:1337/api/playbooks/test',
				expect.objectContaining({
					method: 'PUT',
					headers: expect.objectContaining({
						'Content-Type': 'text/markdown'
					})
				})
			);
		});

		it('deletePlaybook sends DELETE', async () => {
			mockJsonResponse({ ok: true });
			await client.deletePlaybook('test');
			expect(mockFetch).toHaveBeenCalledWith(
				'http://localhost:1337/api/playbooks/test',
				expect.objectContaining({
					method: 'DELETE'
				})
			);
		});

		it('encodes playbook names in URL', async () => {
			mockJsonResponse({ ok: true });
			await client.getPlaybook('my-playbook');
			expect(mockFetch).toHaveBeenCalledWith(
				'http://localhost:1337/api/playbooks/my-playbook',
				expect.any(Object)
			);
		});
	});

	describe('error handling', () => {
		it('throws on non-ok response', async () => {
			mockFetch.mockResolvedValueOnce({
				ok: false,
				status: 404,
				statusText: 'Not Found',
				text: () => Promise.resolve('Not found')
			});
			await expect(client.getInfo()).rejects.toThrow('404: Not found');
		});
	});
});
