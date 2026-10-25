import { describe, it, expect, afterEach, vi } from 'vitest';
import { render } from '@testing-library/svelte';
import TerminalContainer from './TerminalContainer.svelte';
import { SctlWsClient } from '../utils/ws-client';
import type { WsSessionOutputMsg } from '../types/terminal.types';

/** What the terminal is given to write, in order. */
const terminal = vi.hoisted(() => ({ written: [] as string[] }));

// xterm needs a browser's layout, so the Terminal component gets a stand-in
// that keeps what it is given to write.
vi.mock('../utils/xterm', () => ({
	createTerminal: async () => ({
		terminal: {
			write: (data: string) => {
				terminal.written.push(data);
			},
			onData: () => ({ dispose() {} }),
			onResize: () => ({ dispose() {} }),
			focus() {},
			clear() {},
			rows: 24,
			cols: 80,
			options: {}
		},
		fitAddon: { fit() {} },
		searchAddon: { findNext() {}, findPrevious() {}, clearDecorations() {} },
		dispose() {}
	}),
	applyTheme() {}
}));

/** jsdom has no layout: an observed element reports a size at once. */
class SizedResizeObserver {
	constructor(private readonly callback: (entries: { contentRect: { width: number; height: number } }[]) => void) {}

	observe(): void {
		this.callback([{ contentRect: { width: 640, height: 480 } }]);
	}

	unobserve(): void {}

	disconnect(): void {}
}

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
	onSend: ((frame: Frame) => void) | null = null;

	constructor(readonly url: string) {
		FakeSocket.last = this;
	}

	send(text: string): void {
		this.onSend?.(JSON.parse(text) as Frame);
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

afterEach(() => {
	terminal.written.length = 0;
	vi.unstubAllGlobals();
});

describe('TerminalContainer', () => {
	it('listens before it attaches with dedupes, and writes each line once, in order', async () => {
		vi.stubGlobal('ResizeObserver', SizedResizeObserver);
		vi.stubGlobal('WebSocket', FakeSocket);
		const wsUrl = 'ws://192.0.2.1:1337/api/ws';
		const client = new SctlWsClient(wsUrl, 'test-key');
		client.connect();
		const socket = FakeSocket.last!;
		socket.open();
		let attach: Frame | null = null;
		socket.onSend = (frame) => {
			if (frame.type === 'session.list') {
				socket.deliver({ type: 'session.listed', sessions: [], request_id: frame.request_id });
			}
			if (frame.type !== 'session.attach') return;
			attach = frame;
			// Live output as the attach leaves, above where its replay will
			// end: it reaches the terminal only if the container listened
			// before it attached, and only after the replay.
			socket.deliver(out(4));
		};

		const { component } = render(TerminalContainer, {
			props: { config: { wsUrl, apiKey: 'test-key', client, autoStartSession: false } }
		});
		const attached = component.attachSession('s1');
		expect(attach).toMatchObject({ type: 'session.attach', session_id: 's1', since: 0, dedupes: true });

		socket.deliver({
			type: 'session.attached',
			session_id: 's1',
			entries: [out(1), out(2), out(3)],
			dropped: 0,
			request_id: attach!.request_id
		});
		await attached;
		// Later live output: the replay's last line again, then a new one.
		socket.deliver(out(3));
		socket.deliver(out(5));

		const lines = ['line 1\n', 'line 2\n', 'line 3\n', 'line 4\n', 'line 5\n'];
		await vi.waitFor(() => expect(terminal.written).toEqual(lines));
	});
});
