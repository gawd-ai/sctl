import { describe, it, expect } from 'vitest';
import {
	formParams,
	isServerReference,
	parsePlaybookFrontmatter,
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

	it('leaves {{agent}} as written: the server that sends the script fills it', () => {
		const withAgent = '{{agent}} net tcp {{radio}} 80\n{{agent}} install-info';
		expect(renderPlaybookScript(withAgent, { radio: '192.168.1.1' }, params)).toBe(
			'{{agent}} net tcp 192.168.1.1 80\n{{agent}} install-info'
		);
		// With no parameter at all, too.
		expect(renderPlaybookScript('{{agent}} net state', {}, {})).toBe('{{agent}} net state');
	});

	it('still refuses every other undeclared placeholder, beside {{agent}} or after it', () => {
		expect(() => renderPlaybookScript('{{agent}} net tcp {{host}} 80', {}, {})).toThrow(
			'Script references undeclared parameter: {{host}}'
		);
		expect(() => renderPlaybookScript('echo {{agents}}', {}, {})).toThrow(
			'undeclared parameter: {{agents}}'
		);
	});

	it('never fills {{agent}} from the form, even when a playbook declares it', () => {
		const declared = {
			agent: { type: 'string', description: 'A path', default: '/bin/false' },
			radio: { type: 'string', description: 'Which radio', default: 'all' }
		};
		expect(
			renderPlaybookScript('{{agent}} net wifi {{radio}}', { agent: '/tmp/evil' }, declared)
		).toBe('{{agent}} net wifi all');
		// A declared agent with no default is not a missing parameter.
		const noDefault = { agent: { type: 'string', description: 'A path' } };
		expect(renderPlaybookScript('{{agent}} net state', {}, noDefault)).toBe(
			'{{agent}} net state'
		);
	});
});

describe('formParams', () => {
	it('asks for every declared parameter but a server reference, sorted', () => {
		const params = {
			radio: { type: 'string', description: 'Which radio', default: 'all' },
			agent: { type: 'string', description: 'A path' },
			delay: { type: 'number', description: 'Delay', default: 5 }
		};
		expect(formParams(params).map(([name]) => name)).toEqual(['delay', 'radio']);
		expect(formParams({})).toEqual([]);
		expect(isServerReference('agent')).toBe(true);
		expect(isServerReference('radio')).toBe(false);
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
