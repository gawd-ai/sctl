import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ControlBar from './ControlBar.svelte';

/** The Allow AI control: its button, its state as shown, and its title. */
function allowAi(): { pressed: string | null; state: string | null; title: string } {
	const button = screen.getByRole('button', { name: 'Allow AI' });
	expect(button.textContent).toContain('Allow AI');
	return {
		pressed: button.getAttribute('aria-pressed'),
		state: button.querySelector('[data-testid="allow-ai-state"]')?.textContent ?? null,
		title: button.getAttribute('title') ?? ''
	};
}

/** A dash a title must not hold: an em dash, an en dash, or a hyphen between words. */
const DASH = /[\u2013\u2014]| - /;

describe('ControlBar', () => {
	it('labels Allow AI and shows whether it is on', async () => {
		const { rerender } = render(ControlBar, { props: { userAllowsAi: false, aiIsWorking: false } });
		expect(allowAi()).toMatchObject({ pressed: 'false', state: 'off' });

		await rerender({ userAllowsAi: true, aiIsWorking: false });
		expect(allowAi()).toMatchObject({ pressed: 'true', state: 'on' });

		await rerender({ userAllowsAi: true, aiIsWorking: true, aiActivity: 'write' });
		expect(allowAi()).toMatchObject({ pressed: 'true', state: 'on' });
	});

	it('titles every state of Allow AI without a dash', async () => {
		const states = [
			{ userAllowsAi: false, aiIsWorking: false },
			{ userAllowsAi: true, aiIsWorking: false },
			{ userAllowsAi: true, aiIsWorking: true, aiActivity: 'read' },
			{ userAllowsAi: true, aiIsWorking: true, aiActivity: 'write' },
			{ userAllowsAi: false, aiIsWorking: false, disabled: true }
		];
		const { rerender } = render(ControlBar, { props: states[0] });
		const titles: string[] = [];
		for (const props of states) {
			await rerender(props);
			titles.push(allowAi().title);
		}
		expect(titles).toEqual([
			'AI clients may not type in this session. Click to allow AI.',
			'AI allowed, standing by. Click to stop allowing AI.',
			'AI is reading. Click to stop allowing AI.',
			'AI is executing. Click to stop allowing AI.',
			'Session detached'
		]);
		for (const title of titles) expect(title).not.toMatch(DASH);
	});
});
