/**
 * A session's output, each entry written once, by its seq.
 *
 * A client that sends `dedupes: true` with `session.attach` writes an entry
 * only when its seq is above the highest it holds for the session, and
 * listens for the session's output before it attaches. Through a relay, an
 * agent from 0.6.12 then keeps the output stream the clients already
 * watching share, and replays up to where that stream stands, so live output
 * at or below the replay's end may arrive before or after the reply. Live
 * output that arrives before the reply is held, then written after the
 * replay.
 */

import type { WsSessionAttachedMsg } from '../types/terminal.types';
import type { SctlWsClient } from './ws-client';

/** An entry as the agent numbers it. A live message may carry several entries under the seq of its last. */
export interface SeqEntry {
	seq: number;
	data: string;
}

/**
 * The entries to write, in order, and the session's highest seq after them:
 * each entry only when its seq is above the highest so far.
 */
export function entriesToWrite<T extends SeqEntry>(
	highest: number,
	entries: readonly T[]
): { write: T[]; highest: number } {
	const write: T[] = [];
	let top = highest;
	for (const entry of entries) {
		if (entry.seq > top) {
			write.push(entry);
			top = entry.seq;
		}
	}
	return { write, highest: top };
}

/**
 * Each session's highest written seq, and the live output held while its
 * attach is in flight. Live output and the flush after an attach both go
 * through `entriesToWrite`.
 */
export class SessionSeq<T extends SeqEntry = SeqEntry> {
	private readonly top = new Map<string, number>();
	private readonly held = new Map<string, T[]>();

	/** The highest seq written for the session, if any. */
	highest(sessionId: string): number | undefined {
		return this.top.get(sessionId);
	}

	/** Hold the session's live output: its attach is about to be sent. */
	hold(sessionId: string): void {
		if (!this.held.has(sessionId)) this.held.set(sessionId, []);
	}

	/** Live output: what to write now. Nothing while the session's attach is in flight: the entry is held. */
	live(sessionId: string, entry: T): T[] {
		const held = this.held.get(sessionId);
		if (held) {
			held.push(entry);
			return [];
		}
		return this.admit(sessionId, [entry]);
	}

	/** The attach answered: the replay, then the held output, each entry only when its seq is above the highest written. Holding ends. */
	settle(sessionId: string, replay: readonly T[]): T[] {
		const held = this.held.get(sessionId) ?? [];
		this.held.delete(sessionId);
		return this.admit(sessionId, [...replay, ...held]);
	}

	/** The attach failed: holding ends, and what was held is dropped. */
	release(sessionId: string): void {
		this.held.delete(sessionId);
	}

	/** Forget the session: its highest seq and anything held. */
	forget(sessionId: string): void {
		this.top.delete(sessionId);
		this.held.delete(sessionId);
	}

	private admit(sessionId: string, entries: readonly T[]): T[] {
		const { write, highest } = entriesToWrite(this.top.get(sessionId) ?? 0, entries);
		this.top.set(sessionId, highest);
		return write;
	}
}

/**
 * Attach as a client that keeps output by seq: hold the session's live
 * output, register its listener with `listen`, then send the attach with
 * `dedupes: true` from the highest seq written. The listener is in place
 * before the attach leaves. The caller writes `seq.settle(sessionId,
 * reply.entries)` once it can; on a failure, holding ends.
 */
export async function attachKeepingSeq<T extends SeqEntry>(
	client: Pick<SctlWsClient, 'attachSession'>,
	seq: SessionSeq<T>,
	sessionId: string,
	listen: () => void
): Promise<WsSessionAttachedMsg> {
	seq.hold(sessionId);
	try {
		listen();
		return await client.attachSession(sessionId, seq.highest(sessionId) ?? 0, { dedupes: true });
	} catch (err) {
		seq.release(sessionId);
		throw err;
	}
}
