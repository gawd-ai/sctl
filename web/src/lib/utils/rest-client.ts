import type { DeviceInfo, DirEntry, FileContent, ExecResult, ActivityEntry, PlaybookSummary, PlaybookDetail, StpInitDownloadResult, StpInitUploadResult, StpChunkAck, StpResumeResult, StpStatusResult, StpListResult, CachedExecResult } from '../types/terminal.types';

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
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				throw new Error(`Request timed out after 30s: ${path}`);
			}
			throw e;
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

	async readFile(path: string, opts?: { offset?: number; limit?: number }): Promise<FileContent> {
		const params = new URLSearchParams({ path });
		if (opts?.offset) params.set('offset', String(opts.offset));
		if (opts?.limit) params.set('limit', String(opts.limit));
		const res = await this.fetch(`/api/files?${params}`);
		return res.json();
	}

	async writeFile(
		path: string,
		content: string,
		opts?: { mode?: string; create_dirs?: boolean }
	): Promise<void> {
		await this.fetch('/api/files', {
			method: 'PUT',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ path, content, ...opts })
		});
	}

	async deleteFile(path: string): Promise<void> {
		await this.fetch('/api/files', {
			method: 'DELETE',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ path })
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

	async getHealth(): Promise<{ status: string; uptime: number; version: string }> {
		const res = await this.fetch('/api/health');
		return res.json();
	}

	async listPlaybooks(): Promise<PlaybookSummary[]> {
		const res = await this.fetch('/api/playbooks');
		const data = await res.json();
		return data.playbooks ?? [];
	}

	async getPlaybook(name: string): Promise<PlaybookDetail> {
		const res = await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`);
		return res.json();
	}

	async putPlaybook(name: string, content: string): Promise<void> {
		await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`, {
			method: 'PUT',
			headers: { 'Content-Type': 'text/markdown' },
			body: content
		});
	}

	async deletePlaybook(name: string): Promise<void> {
		await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`, {
			method: 'DELETE'
		});
	}

	downloadUrl(path: string): string {
		const params = new URLSearchParams({ path });
		return `${this.baseUrl}/api/files/raw?${params}`;
	}

	async downloadBlob(path: string): Promise<{ blob: Blob; filename: string }> {
		const params = new URLSearchParams({ path });
		const res = await this.fetch(`/api/files/raw?${params}`);
		const blob = await res.blob();
		const filename = path.split('/').pop() ?? 'download';
		return { blob, filename };
	}

	async uploadFiles(dirPath: string, files: FileList | File[]): Promise<void> {
		const form = new FormData();
		for (const file of files) {
			form.append('files', file, file.name);
		}
		const params = new URLSearchParams({ path: dirPath });
		await this.fetch(`/api/files/upload?${params}`, {
			method: 'POST',
			body: form
		});
	}

	async getExecResult(activityId: number): Promise<CachedExecResult> {
		const res = await this.fetch(`/api/activity/${activityId}/result`);
		return res.json();
	}

	// ── STP (chunked transfer) methods ────────────────────────────

	async stpInitDownload(path: string, chunkSize?: number): Promise<StpInitDownloadResult> {
		const body: Record<string, unknown> = { path };
		if (chunkSize) body.chunk_size = chunkSize;
		const res = await this.fetch('/api/stp/download', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify(body)
		});
		return res.json();
	}

	async stpInitUpload(req: {
		path: string; filename: string; file_size: number;
		file_hash: string; chunk_size: number; total_chunks: number; mode?: string;
	}): Promise<StpInitUploadResult> {
		const res = await this.fetch('/api/stp/upload', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify(req)
		});
		return res.json();
	}

	async stpGetChunk(transferId: string, index: number, signal?: AbortSignal): Promise<{ data: ArrayBuffer; hash: string }> {
		const url = `${this.baseUrl}/api/stp/chunk/${encodeURIComponent(transferId)}/${index}`;
		const headers: Record<string, string> = {};
		if (this.apiKey) headers['Authorization'] = `Bearer ${this.apiKey}`;
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), 60000);
		signal?.addEventListener('abort', () => controller.abort(), { once: true });
		try {
			const res = await fetch(url, { headers, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new Error(`${res.status}: ${text}`);
			}
			const hash = res.headers.get('X-Gx-Chunk-Hash') ?? '';
			const data = await res.arrayBuffer();
			return { data, hash };
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				if (signal?.aborted) throw new Error('Transfer aborted');
				throw new Error('Chunk download timed out after 60s');
			}
			throw e;
		} finally {
			clearTimeout(timeout);
		}
	}

	async stpSendChunk(transferId: string, index: number, data: ArrayBuffer, hash: string, signal?: AbortSignal): Promise<StpChunkAck> {
		const url = `${this.baseUrl}/api/stp/chunk/${encodeURIComponent(transferId)}/${index}`;
		const headers: Record<string, string> = {
			'Content-Type': 'application/octet-stream',
			'X-Gx-Chunk-Hash': hash
		};
		if (this.apiKey) headers['Authorization'] = `Bearer ${this.apiKey}`;
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), 60000);
		signal?.addEventListener('abort', () => controller.abort(), { once: true });
		try {
			const res = await fetch(url, { method: 'POST', headers, body: data, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new Error(`${res.status}: ${text}`);
			}
			return res.json();
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				if (signal?.aborted) throw new Error('Transfer aborted');
				throw new Error('Chunk upload timed out after 60s');
			}
			throw e;
		} finally {
			clearTimeout(timeout);
		}
	}

	async stpResume(transferId: string): Promise<StpResumeResult> {
		const res = await this.fetch(`/api/stp/resume/${encodeURIComponent(transferId)}`, { method: 'POST' });
		return res.json();
	}

	async stpAbort(transferId: string): Promise<void> {
		await this.fetch(`/api/stp/${encodeURIComponent(transferId)}`, { method: 'DELETE' });
	}

	async stpStatus(transferId: string): Promise<StpStatusResult> {
		const res = await this.fetch(`/api/stp/status/${encodeURIComponent(transferId)}`);
		return res.json();
	}

	async stpList(): Promise<StpListResult> {
		const res = await this.fetch('/api/stp/transfers');
		return res.json();
	}
}
