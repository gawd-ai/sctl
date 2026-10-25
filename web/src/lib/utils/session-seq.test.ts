import { describe, it, expect, afterEach, vi } from 'vitest';
import { SessionSeq, attachKeepingSeq, entriesToWrite } from './session-seq';
import { SctlWsClient } from './ws-client';
import type { WsSessionOutputMsg } from '../types/terminal.types';

/** `line <seq>`, as the agent sends it for session `s1`. */
function out(seq: number): WsSessionOutputMsg {
	return { type: 'session.stdout', session_id: 's1', data: `line ${seq}\n`, seq, timestamp_ms: 0 };
}

type Frame = Record<string, unknown>;

/** A WebSocket the test drives: it records what the client sends and delivers what the test says. */
class FakeSocket {
	static readonly CONNECTING = 0;
	static readonly OPEN = 1;
	static readonly CLOSING = 2;
	static readonly CLOSED = 3;
	static last: FakeSocket | null = null;

	readyState = FakeSocket.CONNECTING;
	onopen: (() => void) | null = null;
	onmessage: ((event: { data: string }) => void) | null = null;
	onclose: (() => void) | null = null;
	onerror: (() => void) | null = null;
	sent: Frame[] = [];
	onSend: ((frame: Frame) => void) | null = null;

	constructor(readonly url: string) {
		FakeSocket.last = this;
	}

	send(text: string): void {
		const frame = JSON.parse(text) as Frame;
		this.sent.push(frame);
		this.onSend?.(frame);
	}

	close(): void {
		this.readyState = FakeSocket.CLOSED;
	}

	open(): void {
		this.readyState = FakeSocket.OPEN;
		this.onopen?.();
	}

	deliver(msg: unknown): void {
		this.onmessage?.({ data: JSON.stringify(msg) });
	}
}

let client: SctlWsClient | null = null;

/** A client connected to a fake socket. */
function connected(): { client: SctlWsClient; socket: FakeSocket } {
	vi.stubGlobal('WebSocket', FakeSocket);
	client = new SctlWsClient('ws://192.0.2.1:1337/api/ws', 'test-key');
	client.connect();
	const socket = FakeSocket.last!;
	socket.open();
	return { client, socket };
}

afterEach(() => {
	client?.disconnect();
	client = null;
	vi.unstubAllGlobals();
});

describe('entriesToWrite', () => {
	it('writes each entry once, above the highest so far', () => {
		const { write, highest } = entriesToWrite(0, [out(1), out(2), out(2), out(1), out(3)]);
		expect(write.map((e) => e.seq)).toEqual([1, 2, 3]);
		expect(highest).toBe(3);
	});

	it('keeps the highest when nothing is above it', () => {
		const { write, highest } = entriesToWrite(5, [out(4), out(5)]);
		expect(write).toEqual([]);
		expect(highest).toBe(5);
	});
});

describe('SessionSeq', () => {
	it('holds live output during an attach and writes it once after the replay', () => {
		const seq = new SessionSeq<WsSessionOutputMsg>();
		const written: string[] = [];
		const write = (entries: WsSessionOutputMsg[]) => entries.forEach((e) => written.push(e.data));

		seq.hold('s1');
		// A live entry before the reply, which the replay also carries.
		write(seq.live('s1', out(3)));
		expect(written).toEqual([]);
		write(seq.settle('s1', [out(1), out(2), out(3)]));
		// Later live output, a repeat of the replay's last batch first.
		write(seq.live('s1', out(3)));
		write(seq.live('s1', out(4)));
		expect(written).toEqual(['line 1\n', 'line 2\n', 'line 3\n', 'line 4\n']);
		expect(seq.highest('s1')).toBe(4);
	});

	it('writes held output above the replay after it', () => {
		const seq = new SessionSeq<WsSessionOutputMsg>();
		seq.hold('s1');
		seq.live('s1', out(3));
		expect(seq.settle('s1', [out(1), out(2)]).map((e) => e.seq)).toEqual([1, 2, 3]);
	});

	it('ends holding on a failed attach and drops what was held', () => {
		const seq = new SessionSeq<WsSessionOutputMsg>();
		seq.hold('s1');
		seq.live('s1', out(1));
		seq.release('s1');
		expect(seq.live('s1', out(2)).map((e) => e.seq)).toEqual([2]);
	});

	it('forgets a session', () => {
		const seq = new SessionSeq<WsSessionOutputMsg>();
		seq.live('s1', out(7));
		seq.forget('s1');
		expect(seq.highest('s1')).toBeUndefined();
		expect(seq.live('s1', out(1)).map((e) => e.seq)).toEqual([1]);
	});
});

describe('attachKeepingSeq', () => {
	it('listens before the attach is sent, and writes each line once, in order', async () => {
		const { client, socket } = connected();
		const seq = new SessionSeq<WsSessionOutputMsg>();
		const written: string[] = [];
		const write = (entries: WsSessionOutputMsg[]) => entries.forEach((e) => written.push(e.data));
		const events: string[] = [];
		let attach: Frame | null = null;
		socket.onSend = (frame) => {
			if (frame.type !== 'session.attach') return;
			events.push('attach sent');
			attach = frame;
			// Live output as the attach leaves: a batch the replay also carries.
			socket.deliver(out(3));
		};

		const reply = attachKeepingSeq(client, seq, 's1', () => {
			events.push('listening');
			client.onOutput('s1', (msg) => write(seq.live('s1', msg)));
		});
		expect(events).toEqual(['listening', 'attach sent']);
		expect(attach).toMatchObject({ type: 'session.attach', session_id: 's1', since: 0, dedupes: true });

		socket.deliver({
			type: 'session.attached',
			session_id: 's1',
			entries: [out(1), out(2), out(3)],
			dropped: 0,
			request_id: attach!.request_id
		});
		const attached = await reply;
		expect(written).toEqual([]);
		write(seq.settle('s1', attached.entries));
		socket.deliver(out(3));
		socket.deliver(out(4));
		expect(written).toEqual(['line 1\n', 'line 2\n', 'line 3\n', 'line 4\n']);
	});

	it('attaches again from the highest seq written', async () => {
		const { client, socket } = connected();
		const seq = new SessionSeq<WsSessionOutputMsg>();
		seq.live('s1', out(1));
		seq.live('s1', out(2));
		const reply = attachKeepingSeq(client, seq, 's1', () => {});
		const attach = socket.sent.find((f) => f.type === 'session.attach')!;
		expect(attach.since).toBe(2);
		socket.deliver({ type: 'session.attached', session_id: 's1', entries: [out(3)], dropped: 0, request_id: attach.request_id });
		expect(seq.settle('s1', (await reply).entries).map((e) => e.seq)).toEqual([3]);
	});

	it('ends holding when the attach fails', async () => {
		const { client, socket } = connected();
		const seq = new SessionSeq<WsSessionOutputMsg>();
		const reply = attachKeepingSeq(client, seq, 's1', () => {});
		const attach = socket.sent.find((f) => f.type === 'session.attach')!;
		socket.deliver({
			type: 'error',
			code: 'SESSION_NOT_FOUND',
			message: 'Session s1 not found',
			session_id: 's1',
			request_id: attach.request_id
		});
		await expect(reply).rejects.toThrow('Session s1 not found');
		expect(seq.live('s1', out(1)).map((e) => e.seq)).toEqual([1]);
	});
});

describe('SctlWsClient.attachSession', () => {
	it('sends dedupes only when asked', () => {
		const { client, socket } = connected();
		void client.attachSession('s1', 5).catch(() => {});
		void client.attachSession('s1', 5, { dedupes: true }).catch(() => {});
		const attaches = socket.sent.filter((f) => f.type === 'session.attach');
		expect(attaches[0]).not.toHaveProperty('dedupes');
		expect(attaches[1]).toMatchObject({ since: 5, dedupes: true });
	});
});
