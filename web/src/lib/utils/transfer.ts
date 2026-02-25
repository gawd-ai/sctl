/**
 * Client-side chunked transfer engine using gawdxfer (STP) protocol.
 *
 * All file transfers — regardless of size — go through chunked STP endpoints.
 * Provides progress callbacks, per-chunk SHA-256 verification, and retry logic.
 */

import type { SctlRestClient } from './rest-client';
import type { TransferDirection } from '../types/terminal.types';
import { TransferError } from './errors';

const MAX_CHUNK_RETRIES = 3;
const DEFAULT_CHUNK_SIZE = 262_144; // 256 KiB

export interface TransferProgress {
	transferId: string;
	direction: TransferDirection;
	filename: string;
	fileSize: number;
	chunksDone: number;
	totalChunks: number;
	bytesTransferred: number;
	rateBps: number;
	elapsedMs: number;
	/** 0..1 */
	fraction: number;
}

export type TransferState = 'active' | 'complete' | 'error' | 'aborted';

export interface ClientTransfer {
	transferId: string;
	direction: TransferDirection;
	filename: string;
	fileSize: number;
	state: TransferState;
	progress: TransferProgress;
	error?: string;
}

export type OnProgress = (p: TransferProgress) => void;
export type OnComplete = (t: ClientTransfer) => void;
export type OnError = (t: ClientTransfer, msg: string) => void;

/** Compute SHA-256 of an ArrayBuffer via Web Crypto. Returns lowercase hex. */
async function sha256Hex(data: ArrayBuffer): Promise<string> {
	const hash = await crypto.subtle.digest('SHA-256', data);
	return Array.from(new Uint8Array(hash))
		.map((b) => b.toString(16).padStart(2, '0'))
		.join('');
}

export class TransferTracker {
	private transfers = new Map<string, ClientTransfer>();
	private restClient: SctlRestClient;
	private abortControllers = new Map<string, AbortController>();

	onprogress?: OnProgress;
	oncomplete?: OnComplete;
	onerror?: OnError;
	/** Called whenever the transfers map changes (add/remove/update). */
	onchange?: () => void;

	constructor(restClient: SctlRestClient) {
		this.restClient = restClient;
	}

	get activeTransfers(): ClientTransfer[] {
		return [...this.transfers.values()];
	}

	get hasActive(): boolean {
		return [...this.transfers.values()].some((t) => t.state === 'active');
	}

	/** Download a file from the device. Returns a Blob on success. */
	async download(path: string, onProgress?: OnProgress): Promise<Blob> {
		const init = await this.restClient.stpInitDownload(path, DEFAULT_CHUNK_SIZE);
		const { transfer_id, file_size, file_hash, chunk_size, total_chunks, filename } = init;

		const ac = new AbortController();
		this.abortControllers.set(transfer_id, ac);

		const ct: ClientTransfer = {
			transferId: transfer_id,
			direction: 'download',
			filename,
			fileSize: file_size,
			state: 'active',
			progress: makeProgress(transfer_id, 'download', filename, file_size, total_chunks)
		};
		this.transfers.set(transfer_id, ct);
		this.onchange?.();

		const startTime = performance.now();
		const chunks: ArrayBuffer[] = new Array(total_chunks);
		let bytesTransferred = 0;

		try {
			for (let i = 0; i < total_chunks; i++) {
				if (ac.signal.aborted) throw new Error('Transfer aborted');

				let lastErr: Error | null = null;
				for (let retry = 0; retry < MAX_CHUNK_RETRIES; retry++) {
					try {
						const { data, hash } = await this.restClient.stpGetChunk(transfer_id, i, ac.signal);

						// Verify chunk hash
						const actual = await sha256Hex(data);
						if (actual !== hash) {
							throw new TransferError(`Chunk ${i} hash mismatch: expected ${hash}, got ${actual}`, transfer_id);
						}

						chunks[i] = data;
						bytesTransferred += data.byteLength;
						lastErr = null;
						break;
					} catch (e) {
						lastErr = e instanceof Error ? e : new Error(String(e));
						if (ac.signal.aborted) throw lastErr;
					}
				}
				if (lastErr) throw lastErr;

				// Update progress
				const elapsed = performance.now() - startTime;
				ct.progress = {
					transferId: transfer_id,
					direction: 'download',
					filename,
					fileSize: file_size,
					chunksDone: i + 1,
					totalChunks: total_chunks,
					bytesTransferred,
					rateBps: elapsed > 0 ? Math.round((bytesTransferred * 1000) / elapsed) : 0,
					elapsedMs: Math.round(elapsed),
					fraction: (i + 1) / total_chunks
				};
				this.onprogress?.(ct.progress);
				onProgress?.(ct.progress);
				this.onchange?.();
			}

			// Assemble blob and verify whole-file hash
			const fullBuf = new Uint8Array(bytesTransferred);
			let off = 0;
			for (const c of chunks) {
				fullBuf.set(new Uint8Array(c), off);
				off += c.byteLength;
			}

			const actualHash = await sha256Hex(fullBuf.buffer);
			if (actualHash !== file_hash) {
				throw new TransferError(`File hash mismatch: expected ${file_hash}, got ${actualHash}`, transfer_id);
			}

			ct.state = 'complete';
			ct.progress.fraction = 1;
			this.oncomplete?.(ct);
			this.onchange?.();

			// Auto-remove after 5s
			setTimeout(() => {
				this.transfers.delete(transfer_id);
				this.onchange?.();
			}, 5000);

			return new Blob([fullBuf], { type: 'application/octet-stream' });
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Download failed';
			ct.state = ac.signal.aborted ? 'aborted' : 'error';
			ct.error = msg;
			this.onerror?.(ct, msg);
			this.onchange?.();
			throw err;
		} finally {
			this.abortControllers.delete(transfer_id);
		}
	}

	/** Upload a file to the device at the given directory path. */
	async upload(dirPath: string, file: File, onProgress?: OnProgress): Promise<void> {
		const chunkSize = DEFAULT_CHUNK_SIZE;
		const totalChunks = Math.max(1, Math.ceil(file.size / chunkSize));

		const init = await this.restClient.stpInitUpload({
			path: dirPath,
			filename: file.name,
			file_size: file.size,
			file_hash: '',
			chunk_size: chunkSize,
			total_chunks: totalChunks
		});

		const { transfer_id } = init;
		const ac = new AbortController();
		this.abortControllers.set(transfer_id, ac);

		const ct: ClientTransfer = {
			transferId: transfer_id,
			direction: 'upload',
			filename: file.name,
			fileSize: file.size,
			state: 'active',
			progress: makeProgress(transfer_id, 'upload', file.name, file.size, totalChunks)
		};
		this.transfers.set(transfer_id, ct);
		this.onchange?.();

		const startTime = performance.now();
		let bytesTransferred = 0;

		try {
			for (let i = 0; i < totalChunks; i++) {
				if (ac.signal.aborted) throw new Error('Transfer aborted');

				const offset = i * chunkSize;
				const end = Math.min(offset + chunkSize, file.size);
				const slice = file.slice(offset, end);
				const buf = await slice.arrayBuffer();
				const chunkHash = await sha256Hex(buf);

				let lastErr: Error | null = null;
				for (let retry = 0; retry < MAX_CHUNK_RETRIES; retry++) {
					try {
						const ack = await this.restClient.stpSendChunk(transfer_id, i, buf, chunkHash, ac.signal);
						if (!ack.ok) {
							throw new TransferError(ack.error ?? `Chunk ${i} rejected`, transfer_id);
						}
						lastErr = null;
						break;
					} catch (e) {
						lastErr = e instanceof Error ? e : new Error(String(e));
						if (ac.signal.aborted) throw lastErr;
					}
				}
				if (lastErr) throw lastErr;

				bytesTransferred += buf.byteLength;
				const elapsed = performance.now() - startTime;
				ct.progress = {
					transferId: transfer_id,
					direction: 'upload',
					filename: file.name,
					fileSize: file.size,
					chunksDone: i + 1,
					totalChunks,
					bytesTransferred,
					rateBps: elapsed > 0 ? Math.round((bytesTransferred * 1000) / elapsed) : 0,
					elapsedMs: Math.round(elapsed),
					fraction: (i + 1) / totalChunks
				};
				this.onprogress?.(ct.progress);
				onProgress?.(ct.progress);
				this.onchange?.();
			}

			ct.state = 'complete';
			ct.progress.fraction = 1;
			this.oncomplete?.(ct);
			this.onchange?.();

			// Auto-remove after 5s
			setTimeout(() => {
				this.transfers.delete(transfer_id);
				this.onchange?.();
			}, 5000);
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Upload failed';
			ct.state = ac.signal.aborted ? 'aborted' : 'error';
			ct.error = msg;
			this.onerror?.(ct, msg);
			this.onchange?.();
			// Attempt cleanup
			try { await this.restClient.stpAbort(transfer_id); } catch { /* ignore */ }
			throw err;
		} finally {
			this.abortControllers.delete(transfer_id);
		}
	}

	/** Abort an active transfer. */
	abort(transferId: string) {
		const ac = this.abortControllers.get(transferId);
		if (ac) ac.abort();
		const ct = this.transfers.get(transferId);
		if (ct && ct.state === 'active') {
			ct.state = 'aborted';
			this.onchange?.();
		}
		this.restClient.stpAbort(transferId).catch(() => {});
	}

	/** Remove a completed/errored/aborted transfer from the list. */
	dismiss(transferId: string) {
		this.transfers.delete(transferId);
		this.onchange?.();
	}

	/** Clear all non-active transfers. */
	clearCompleted() {
		for (const [id, t] of this.transfers) {
			if (t.state !== 'active') this.transfers.delete(id);
		}
		this.onchange?.();
	}
}

function makeProgress(
	transferId: string, direction: TransferDirection,
	filename: string, fileSize: number, totalChunks: number
): TransferProgress {
	return {
		transferId, direction, filename, fileSize,
		chunksDone: 0, totalChunks, bytesTransferred: 0,
		rateBps: 0, elapsedMs: 0, fraction: 0
	};
}
