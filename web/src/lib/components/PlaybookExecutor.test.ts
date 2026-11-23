import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PlaybookExecutor from './PlaybookExecutor.svelte';
import type { PlaybookDetail } from '../types/terminal.types';

/** A playbook whose script names the agent's tools by the reference. */
function playbook(params: PlaybookDetail['params'] = {}): PlaybookDetail {
	return {
		name: 'reach',
		description: 'Reach a host',
		params,
		script: '{{agent}} net tcp {{host}} 80 || echo fallback',
		raw_content: ''
	} as PlaybookDetail;
}

const host = { host: { type: 'string', description: 'Where', default: '192.168.1.1' } };

/** A WebSocket client that records the job it is asked to start. */
function wsStub() {
	const startJob = vi.fn(async (_job: { command: string; name?: string }) => ({
		session_id: 's-1'
	}));
	return {
		client: { startJob, onOutput: () => () => {}, onSessionEnd: () => () => {} },
		startJob
	};
}

/** A REST client that records the exec it is asked to make. */
function restStub() {
	const exec = vi.fn(async (_command: string) => ({
		exit_code: 0,
		stdout: '',
		stderr: '',
		duration_ms: 1
	}));
	return { client: { exec }, exec };
}

const UNDECLARED = 'Script references undeclared parameter: {{agent}}';

describe('PlaybookExecutor and a reference the host server fills', () => {
	it('with no option, refuses {{agent}} before anything is sent, on a job and on an exec', async () => {
		const ws = wsStub();
		const job = render(PlaybookExecutor, {
			props: { playbook: playbook(host), restClient: null, wsClient: ws.client as never }
		});
		await expect(job.component.run()).rejects.toThrow(UNDECLARED);
		expect(ws.startJob).not.toHaveBeenCalled();
		job.unmount();

		const rest = restStub();
		const exec = render(PlaybookExecutor, {
			props: { playbook: playbook(host), restClient: rest.client as never }
		});
		await expect(exec.component.run()).rejects.toThrow(UNDECLARED);
		expect(rest.exec).not.toHaveBeenCalled();
	});

	it('with no option, a declared agent parameter is an ordinary field and is filled from the form', async () => {
		const ws = wsStub();
		const declared = { ...host, agent: { type: 'string', description: 'A path', default: '/usr/bin/sctl' } };
		const { component } = render(PlaybookExecutor, {
			props: { playbook: playbook(declared), restClient: null, wsClient: ws.client as never }
		});
		expect(screen.getByLabelText('agent')).toBeTruthy();
		await component.run();
		expect(ws.startJob).toHaveBeenCalledTimes(1);
		expect(ws.startJob.mock.calls[0][0].command).toBe(
			'/usr/bin/sctl net tcp 192.168.1.1 80 || echo fallback'
		);
	});

	it('leaves the reference as written only on a path the host names, and drops its field there', async () => {
		const ws = wsStub();
		const declared = { ...host, agent: { type: 'string', description: 'A path', default: '/bin/false' } };
		const { component } = render(PlaybookExecutor, {
			props: {
				playbook: playbook(declared),
				restClient: null,
				wsClient: ws.client as never,
				serverReferences: { names: ['agent'], paths: ['job'] }
			}
		});
		expect(screen.queryByLabelText('agent')).toBeNull();
		expect(screen.getByLabelText('host')).toBeTruthy();
		await component.run();
		expect(ws.startJob.mock.calls[0][0].command).toBe(
			'{{agent}} net tcp 192.168.1.1 80 || echo fallback'
		);
	});

	it('refuses on the exec path when the host named only the job path', async () => {
		const rest = restStub();
		const { component } = render(PlaybookExecutor, {
			props: {
				playbook: playbook(host),
				restClient: rest.client as never,
				serverReferences: { names: ['agent'], paths: ['job'] }
			}
		});
		await expect(component.run()).rejects.toThrow(UNDECLARED);
		expect(rest.exec).not.toHaveBeenCalled();

		// Named, it is left as written there too.
		const named = restStub();
		const filled = render(PlaybookExecutor, {
			props: {
				playbook: playbook(host),
				restClient: named.client as never,
				serverReferences: { names: ['agent'], paths: ['job', 'exec'] }
			}
		});
		await filled.component.run();
		expect(named.exec).toHaveBeenCalledWith('{{agent}} net tcp 192.168.1.1 80 || echo fallback');
	});

	it('never sends a script to the terminal with a reference left in it: no server fills that path', async () => {
		const ws = wsStub();
		const onRunInTerminal = vi.fn();
		render(PlaybookExecutor, {
			props: {
				playbook: playbook(host),
				restClient: null,
				wsClient: ws.client as never,
				onRunInTerminal,
				serverReferences: { names: ['agent'], paths: ['job', 'exec'] }
			}
		});
		await fireEvent.click(screen.getByTitle('Send script to active terminal session'));
		expect(onRunInTerminal).not.toHaveBeenCalled();
		expect(screen.getByText(UNDECLARED)).toBeTruthy();
	});

	it('sends a script with no reference to the terminal as before', async () => {
		const onRunInTerminal = vi.fn();
		const plain = { ...playbook(host), script: 'ping -c 1 {{host}}' } as PlaybookDetail;
		render(PlaybookExecutor, {
			props: { playbook: plain, restClient: null, onRunInTerminal }
		});
		await fireEvent.click(screen.getByTitle('Send script to active terminal session'));
		expect(onRunInTerminal).toHaveBeenCalledWith('ping -c 1 192.168.1.1');
	});
});
