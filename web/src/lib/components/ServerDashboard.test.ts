import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ServerDashboard from './ServerDashboard.svelte';

/** A dashboard for a unit that is not connected, with no data yet. */
function props(masterAiEnabled: boolean) {
	return {
		visible: true,
		connectionStatus: 'disconnected' as const,
		deviceInfo: null,
		activity: [],
		restClient: null,
		relayHealth: null,
		isRelay: false,
		relaySerial: null,
		disconnectReason: null,
		lastConnectedAt: null,
		deviceProbe: null,
		connectionLog: [],
		telemetryLog: [],
		masterAiEnabled
	};
}

/** The all-sessions Allow AI control: its state as shown and its title. */
function allowAiInOpenSessions(): { pressed: string | null; state: string | null; title: string } {
	const button = screen.getByRole('button', { name: 'Allow AI in open sessions' });
	expect(button.textContent).toContain('Allow AI');
	return {
		pressed: button.getAttribute('aria-pressed'),
		state: button.querySelector('[data-testid="allow-ai-all-state"]')?.textContent ?? null,
		title: button.getAttribute('title') ?? ''
	};
}

/** A dash a title must not hold: an em dash, an en dash, or a hyphen between words. */
const DASH = /[\u2013\u2014]| - /;

describe('ServerDashboard', () => {
	it('labels Allow AI in open sessions, says whether it is on, and claims no more', async () => {
		const { rerender } = render(ServerDashboard, { props: props(false) });
		const off = allowAiInOpenSessions();
		expect(off).toMatchObject({ pressed: 'false', state: 'off' });
		expect(off.title).toBe('Click to allow AI in every open session. A session started later starts closed.');

		await rerender(props(true));
		const on = allowAiInOpenSessions();
		expect(on).toMatchObject({ pressed: 'true', state: 'on' });
		expect(on.title).toBe('AI allowed in every open session. Click to stop allowing AI in all of them.');

		for (const title of [off.title, on.title]) {
			expect(title).not.toMatch(DASH);
			expect(title).not.toContain('all sessions');
		}
	});
});
