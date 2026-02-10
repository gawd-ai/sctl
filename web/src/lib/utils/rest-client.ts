import type { DeviceInfo, DirEntry, FileContent, ExecResult, ActivityEntry } from '../types/terminal.types';

export class SctlRestClient {
	private baseUrl: string;
	private apiKey: string;

	constructor(wsUrl: string, apiKey: string) {
		// Derive HTTP URL from WS URL: ws://host:port/api/ws → http://host:port
		const httpUrl = wsUrl
			.replace(/^wss:/, 'https:')
			.replace(/^ws:/, 'http:')
			.replace(/\/api\/ws\/?$/, '');
		this.baseUrl = httpUrl;
		this.apiKey = apiKey;
	}

	private async fetch(path: string, init?: RequestInit): Promise<Response> {
		const url = `${this.baseUrl}${path}`;
		const headers: Record<string, string> = {
			...(init?.headers as Record<string, string>),
		};
		if (this.apiKey) {
			headers['Authorization'] = `Bearer ${this.apiKey}`;
		}
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), 30000);
		try {
			const res = await fetch(url, { ...init, headers, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new Error(`${res.status}: ${text}`);
			}
			return res;
		} finally {
			clearTimeout(timeout);
		}
	}

	async getInfo(): Promise<DeviceInfo> {
		const res = await this.fetch('/api/info');
		return res.json();
	}

	async listDir(path: string): Promise<DirEntry[]> {
		const params = new URLSearchParams({ path, list: 'true' });
		const res = await this.fetch(`/api/files?${params}`);
		const data = await res.json();
		return data.entries ?? data;
	}

	async readFile(path: string): Promise<FileContent> {
		const params = new URLSearchParams({ path });
		const res = await this.fetch(`/api/files?${params}`);
		return res.json();
	}

	async writeFile(
		path: string,
		content: string,
		opts?: { mode?: string; create_dirs?: boolean }
	): Promise<void> {
		await this.fetch('/api/files', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ path, content, ...opts })
		});
	}

	async exec(
		command: string,
		opts?: { timeout_ms?: number; working_dir?: string; env?: Record<string, string> }
	): Promise<ExecResult> {
		const res = await this.fetch('/api/exec', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ command, ...opts })
		});
		return res.json();
	}

	async getActivity(sinceId = 0, limit = 50): Promise<ActivityEntry[]> {
		const params = new URLSearchParams({
			since_id: sinceId.toString(),
			limit: limit.toString()
		});
		const res = await this.fetch(`/api/activity?${params}`);
		const data = await res.json();
		return data.entries ?? [];
	}
}
