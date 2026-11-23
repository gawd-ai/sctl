import { describe, it, expect } from 'vitest';
import {
	formParams,
	parsePlaybookFrontmatter,
	referencesFilledOn,
	renderPlaybookScript,
	validatePlaybookName
} from './playbook-parser';

const SAMPLE_PLAYBOOK = `---
name: restart-wifi
description: Restart WiFi radio interfaces
params:
  radio:
    type: string
    description: Which radio to restart
    default: all
  delay:
    type: number
    description: Delay in seconds
    default: 5
---
# Restart WiFi

\`\`\`sh
echo "Restarting radio: {{radio}}"
sleep {{delay}}
wifi down {{radio}}
wifi up {{radio}}
\`\`\`
`;

describe('parsePlaybookFrontmatter', () => {
	it('parses a valid playbook', () => {
		const result = parsePlaybookFrontmatter(SAMPLE_PLAYBOOK);
		expect(result.name).toBe('restart-wifi');
		expect(result.description).toBe('Restart WiFi radio interfaces');
		expect(result.params).toHaveProperty('radio');
		expect(result.params).toHaveProperty('delay');
		expect(result.params.radio.type).toBe('string');
		expect(result.params.radio.default).toBe('all');
		expect(result.params.delay.type).toBe('number');
		expect(result.params.delay.default).toBe(5);
		expect(result.script).toContain('wifi down {{radio}}');
		expect(result.raw_content).toBe(SAMPLE_PLAYBOOK);
	});

	it('throws on missing frontmatter', () => {
		expect(() => parsePlaybookFrontmatter('# No frontmatter')).toThrow('Missing YAML frontmatter');
	});

	it('throws on unclosed frontmatter', () => {
		expect(() => parsePlaybookFrontmatter('---\nname: test\n')).toThrow('Missing closing ---');
	});

	it('throws on missing script block', () => {
		const md = `---
name: test
description: A test
---
No code block here.
`;
		expect(() => parsePlaybookFrontmatter(md)).toThrow('No ```sh or ```bash code block found');
	});

	it('throws on empty name', () => {
		const md = `---
name:
description: A test
---
\`\`\`sh
echo hi
\`\`\`
`;
		expect(() => parsePlaybookFrontmatter(md)).toThrow('Playbook name is empty');
	});

	it('handles playbook with no params', () => {
		const md = `---
name: simple
description: A simple playbook
---
\`\`\`sh
echo "hello"
\`\`\`
`;
		const result = parsePlaybookFrontmatter(md);
		expect(result.name).toBe('simple');
		expect(Object.keys(result.params)).toHaveLength(0);
		expect(result.script).toBe('echo "hello"');
	});

	it('handles bash code blocks', () => {
		const md = `---
name: test
description: A test
---
\`\`\`bash
echo "bash block"
\`\`\`
`;
		const result = parsePlaybookFrontmatter(md);
		expect(result.script).toBe('echo "bash block"');
	});
});

describe('renderPlaybookScript', () => {
	const params = {
		radio: { type: 'string', description: 'Which radio', default: 'all' },
		delay: { type: 'number', description: 'Delay', default: 5 }
	};
	const script = 'wifi down {{radio}}\nsleep {{delay}}\nwifi up {{radio}}';

	it('substitutes provided values', () => {
		const result = renderPlaybookScript(script, { radio: 'wlan0', delay: '10' }, params);
		expect(result).toBe('wifi down wlan0\nsleep 10\nwifi up wlan0');
	});

	it('falls back to defaults', () => {
		const result = renderPlaybookScript(script, {}, params);
		expect(result).toBe('wifi down all\nsleep 5\nwifi up all');
	});

	it('throws on missing required param without default', () => {
		const paramsNoDefault = {
			name: { type: 'string', description: 'Required param' }
		};
		expect(() => renderPlaybookScript('echo {{name}}', {}, paramsNoDefault)).toThrow(
			'Missing required parameter: name'
		);
	});

	it('throws on undeclared placeholder', () => {
		expect(() => renderPlaybookScript('echo {{unknown}}', {}, {})).toThrow(
			'undeclared parameter'
		);
	});

	it('refuses {{agent}} like any undeclared placeholder when the host names no reference', () => {
		// As 0.6.13: nothing behind sctlin fills it, so it must not reach a shell.
		expect(() => renderPlaybookScript('{{agent}} net state', {}, {})).toThrow(
			'Script references undeclared parameter: {{agent}}'
		);
		expect(() => renderPlaybookScript('{{agent}} net tcp {{radio}} 80', {}, params)).toThrow(
			'undeclared parameter: {{agent}}'
		);
	});

	it('treats a declared agent parameter as any other when the host names no reference', () => {
		const declared = {
			agent: { type: 'string', description: 'A path', default: '/usr/bin/sctl' },
			radio: { type: 'string', description: 'Which radio', default: 'all' }
		};
		expect(renderPlaybookScript('{{agent}} net wifi {{radio}}', {}, declared)).toBe(
			'/usr/bin/sctl net wifi all'
		);
		expect(
			renderPlaybookScript('{{agent}} net wifi {{radio}}', { agent: '/tmp/sctl' }, declared)
		).toBe('/tmp/sctl net wifi all');
	});

	it('leaves a reference the host names as written, and fills it from no form value', () => {
		const withAgent = '{{agent}} net tcp {{radio}} 80\n{{agent}} install-info';
		expect(renderPlaybookScript(withAgent, { radio: '192.168.1.1' }, params, ['agent'])).toBe(
			'{{agent}} net tcp 192.168.1.1 80\n{{agent}} install-info'
		);
		expect(renderPlaybookScript('{{agent}} net state', {}, {}, ['agent'])).toBe(
			'{{agent}} net state'
		);
		// Even when a playbook declares a parameter of that name.
		const declared = {
			agent: { type: 'string', description: 'A path', default: '/bin/false' },
			radio: { type: 'string', description: 'Which radio', default: 'all' }
		};
		expect(
			renderPlaybookScript(
				'{{agent}} net wifi {{radio}}',
				{ agent: '/tmp/evil' },
				declared,
				['agent']
			)
		).toBe('{{agent}} net wifi all');
	});

	it('still refuses every other undeclared placeholder beside a named reference', () => {
		expect(() =>
			renderPlaybookScript('{{agent}} net tcp {{host}} 80', {}, {}, ['agent'])
		).toThrow('Script references undeclared parameter: {{host}}');
		expect(() => renderPlaybookScript('echo {{agents}}', {}, {}, ['agent'])).toThrow(
			'undeclared parameter: {{agents}}'
		);
	});
});

describe('formParams', () => {
	const params = {
		radio: { type: 'string', description: 'Which radio', default: 'all' },
		agent: { type: 'string', description: 'A path' },
		delay: { type: 'number', description: 'Delay', default: 5 }
	};

	it('asks for every declared parameter, sorted, when the host names no reference', () => {
		expect(formParams(params).map(([name]) => name)).toEqual(['agent', 'delay', 'radio']);
		expect(formParams({})).toEqual([]);
	});

	it('drops the field of a reference the host names', () => {
		expect(formParams(params, ['agent']).map(([name]) => name)).toEqual(['delay', 'radio']);
	});
});

describe('referencesFilledOn', () => {
	it('names the references only on the paths the host says its server fills', () => {
		const host = { names: ['agent'], paths: ['job', 'exec'] as const };
		expect(referencesFilledOn(host, 'job')).toEqual(['agent']);
		expect(referencesFilledOn(host, 'exec')).toEqual(['agent']);
		expect(referencesFilledOn(host, 'terminal')).toEqual([]);
		expect(referencesFilledOn({ names: ['agent'], paths: ['job'] }, 'exec')).toEqual([]);
		expect(referencesFilledOn(undefined, 'job')).toEqual([]);
	});
});

describe('validatePlaybookName', () => {
	it('accepts valid names', () => {
		expect(validatePlaybookName('restart-wifi')).toBe(true);
		expect(validatePlaybookName('my_playbook')).toBe(true);
		expect(validatePlaybookName('test123')).toBe(true);
		expect(validatePlaybookName('A-Z_0-9')).toBe(true);
	});

	it('rejects invalid names', () => {
		expect(validatePlaybookName('')).toBe(false);
		expect(validatePlaybookName('has space')).toBe(false);
		expect(validatePlaybookName('has.dot')).toBe(false);
		expect(validatePlaybookName('has/slash')).toBe(false);
	});
});
