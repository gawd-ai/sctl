import type { DeviceInfo, DirEntry, FileContent, ExecResult, ActivityEntry, PlaybookSummary, PlaybookDetail, StpInitDownloadResult, StpInitUploadResult, StpChunkAck, StpResumeResult, StpStatusResult, StpListResult, CachedExecResult } from '../types/terminal.types';
import { HttpError, TimeoutError } from './errors';

const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_CHUNK_TIMEOUT_MS = 60_000;

/** Configuration for REST client behavior. */
export interface RestClientConfig {
	/** Timeout in ms for standard API requests. Default: 30000. */
	timeout?: number;
	/** Timeout in ms for STP chunk upload/download requests. Default: 60000. */
	chunkTimeout?: number;
}

/**
 * HTTP REST client for sctl device APIs.
 *
 * Derives its base URL from a WebSocket URL (`ws://host:port/api/ws` → `http://host:port`).
 * All requests include Bearer token auth and configurable timeouts.
 */
export class SctlRestClient {
	private baseUrl: string;
	private apiKey: string;
	private readonly timeoutMs: number;
	private readonly chunkTimeoutMs: number;

	constructor(wsUrl: string, apiKey: string, config?: RestClientConfig) {
		// Derive HTTP URL from WS URL: ws://host:port/api/ws → http://host:port
		const httpUrl = wsUrl
			.replace(/^wss:/, 'https:')
			.replace(/^ws:/, 'http:')
			.replace(/\/api\/ws\/?$/, '');
		this.baseUrl = httpUrl;
		this.apiKey = apiKey;
		this.timeoutMs = config?.timeout ?? DEFAULT_TIMEOUT_MS;
		this.chunkTimeoutMs = config?.chunkTimeout ?? DEFAULT_CHUNK_TIMEOUT_MS;
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
		const timeout = setTimeout(() => controller.abort(), this.timeoutMs);
		try {
			const res = await fetch(url, { ...init, headers, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new HttpError(res.status, text);
			}
			return res;
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				throw new TimeoutError(`Request timed out after ${this.timeoutMs}ms: ${path}`);
			}
			throw e;
		} finally {
			clearTimeout(timeout);
		}
	}

	/** Fetch device system information (hostname, CPU, memory, disk, interfaces). */
	async getInfo(): Promise<DeviceInfo> {
		const res = await this.fetch('/api/info');
		return res.json();
	}

	/** List directory contents at the given path. */
	async listDir(path: string): Promise<DirEntry[]> {
		const params = new URLSearchParams({ path, list: 'true' });
		const res = await this.fetch(`/api/files?${params}`);
		const data = await res.json();
		return data.entries ?? data;
	}

	/** Read file content as text (with optional offset/limit for large files). */
	async readFile(path: string, opts?: { offset?: number; limit?: number }): Promise<FileContent> {
		const params = new URLSearchParams({ path });
		if (opts?.offset) params.set('offset', String(opts.offset));
		if (opts?.limit) params.set('limit', String(opts.limit));
		const res = await this.fetch(`/api/files?${params}`);
		return res.json();
	}

	/** Write content to a file on the device. */
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

	/** Delete a file on the device. */
	async deleteFile(path: string): Promise<void> {
		await this.fetch('/api/files', {
			method: 'DELETE',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ path })
		});
	}

	/** Execute a one-shot command on the device (non-interactive). */
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

	/** Fetch the activity log, optionally filtered by since_id and limit. */
	async getActivity(sinceId = 0, limit = 50): Promise<ActivityEntry[]> {
		const params = new URLSearchParams({
			since_id: sinceId.toString(),
			limit: limit.toString()
		});
		const res = await this.fetch(`/api/activity?${params}`);
		const data = await res.json();
		return data.entries ?? [];
	}

	/** Health check — returns status, uptime, and sctl version. No auth required. */
	async getHealth(): Promise<{ status: string; uptime: number; version: string }> {
		const res = await this.fetch('/api/health');
		return res.json();
	}

	/** List all playbooks on the device. */
	async listPlaybooks(): Promise<PlaybookSummary[]> {
		const res = await this.fetch('/api/playbooks');
		const data = await res.json();
		return data.playbooks ?? [];
	}

	/** Get a playbook's full content, parsed frontmatter, and script. */
	async getPlaybook(name: string): Promise<PlaybookDetail> {
		const res = await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`);
		return res.json();
	}

	/** Create or update a playbook (content is raw Markdown with YAML frontmatter). */
	async putPlaybook(name: string, content: string): Promise<void> {
		await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`, {
			method: 'PUT',
			headers: { 'Content-Type': 'text/markdown' },
			body: content
		});
	}

	/** Delete a playbook by name. */
	async deletePlaybook(name: string): Promise<void> {
		await this.fetch(`/api/playbooks/${encodeURIComponent(name)}`, {
			method: 'DELETE'
		});
	}

	/** Get the raw download URL for a file (for use in `<a href>` links). */
	downloadUrl(path: string): string {
		const params = new URLSearchParams({ path });
		return `${this.baseUrl}/api/files/raw?${params}`;
	}

	/** Download a file as a Blob (for programmatic use or save-as dialogs). */
	async downloadBlob(path: string): Promise<{ blob: Blob; filename: string }> {
		const params = new URLSearchParams({ path });
		const res = await this.fetch(`/api/files/raw?${params}`);
		const blob = await res.blob();
		const filename = path.split('/').pop() ?? 'download';
		return { blob, filename };
	}

	/** Upload files to a directory on the device (multipart form upload). */
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

	/** Fetch a cached exec result by activity ID (stdout/stderr/exit code). */
	async getExecResult(activityId: number): Promise<CachedExecResult> {
		const res = await this.fetch(`/api/activity/${activityId}/result`);
		return res.json();
	}

	// ── STP (chunked transfer) methods ────────────────────────────

	/** Initialize a chunked STP download for a file. */
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

	/** Initialize a chunked STP upload for a file. */
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

	/** Download a single chunk by index, with SHA-256 hash verification header. */
	async stpGetChunk(transferId: string, index: number, signal?: AbortSignal): Promise<{ data: ArrayBuffer; hash: string }> {
		const url = `${this.baseUrl}/api/stp/chunk/${encodeURIComponent(transferId)}/${index}`;
		const headers: Record<string, string> = {};
		if (this.apiKey) headers['Authorization'] = `Bearer ${this.apiKey}`;
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), this.chunkTimeoutMs);
		signal?.addEventListener('abort', () => controller.abort(), { once: true });
		try {
			const res = await fetch(url, { headers, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new HttpError(res.status, text);
			}
			const hash = res.headers.get('X-Gx-Chunk-Hash') ?? '';
			const data = await res.arrayBuffer();
			return { data, hash };
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				if (signal?.aborted) throw new Error('Transfer aborted');
				throw new TimeoutError(`Chunk download timed out after ${this.chunkTimeoutMs}ms`);
			}
			throw e;
		} finally {
			clearTimeout(timeout);
		}
	}

	/** Upload a single chunk with its SHA-256 hash for verification. */
	async stpSendChunk(transferId: string, index: number, data: ArrayBuffer, hash: string, signal?: AbortSignal): Promise<StpChunkAck> {
		const url = `${this.baseUrl}/api/stp/chunk/${encodeURIComponent(transferId)}/${index}`;
		const headers: Record<string, string> = {
			'Content-Type': 'application/octet-stream',
			'X-Gx-Chunk-Hash': hash
		};
		if (this.apiKey) headers['Authorization'] = `Bearer ${this.apiKey}`;
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), this.chunkTimeoutMs);
		signal?.addEventListener('abort', () => controller.abort(), { once: true });
		try {
			const res = await fetch(url, { method: 'POST', headers, body: data, signal: controller.signal });
			if (!res.ok) {
				const text = await res.text().catch(() => res.statusText);
				throw new HttpError(res.status, text);
			}
			return res.json();
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') {
				if (signal?.aborted) throw new Error('Transfer aborted');
				throw new TimeoutError(`Chunk upload timed out after ${this.chunkTimeoutMs}ms`);
			}
			throw e;
		} finally {
			clearTimeout(timeout);
		}
	}

	/** Resume a paused/interrupted STP transfer. */
	async stpResume(transferId: string): Promise<StpResumeResult> {
		const res = await this.fetch(`/api/stp/resume/${encodeURIComponent(transferId)}`, { method: 'POST' });
		return res.json();
	}

	/** Abort and clean up an active STP transfer. */
	async stpAbort(transferId: string): Promise<void> {
		await this.fetch(`/api/stp/${encodeURIComponent(transferId)}`, { method: 'DELETE' });
	}

	/** Get the current status of an STP transfer. */
	async stpStatus(transferId: string): Promise<StpStatusResult> {
		const res = await this.fetch(`/api/stp/status/${encodeURIComponent(transferId)}`);
		return res.json();
	}

	/** List all active STP transfers on the device. */
	async stpList(): Promise<StpListResult> {
		const res = await this.fetch('/api/stp/transfers');
		return res.json();
	}
}
