import type { PlaybookDetail, PlaybookParam } from '../types/terminal.types';

interface ParsedFrontmatter {
	name: string;
	description: string;
	params: Record<string, PlaybookParam>;
}

/**
 * Parse YAML frontmatter from a playbook markdown string.
 * Returns name, description, params, and the script block.
 */
export function parsePlaybookFrontmatter(markdown: string): PlaybookDetail {
	const trimmed = markdown.trimStart();
	if (!trimmed.startsWith('---')) {
		throw new Error('Missing YAML frontmatter (must start with ---)');
	}

	const afterOpen = trimmed.slice(3);
	const closePos = afterOpen.indexOf('\n---');
	if (closePos === -1) {
		throw new Error('Missing closing --- for frontmatter');
	}

	const yamlStr = afterOpen.slice(0, closePos);
	const body = afterOpen.slice(closePos + 4);

	const fm = parseSimpleYaml(yamlStr);
	const script = extractScriptBlock(body);

	return {
		name: fm.name,
		description: fm.description,
		params: fm.params,
		script,
		raw_content: markdown
	};
}

/**
 * Minimal YAML frontmatter parser for playbook format.
 * Handles name, description, and params with type/description/default/enum.
 */
function parseSimpleYaml(yaml: string): ParsedFrontmatter {
	const lines = yaml.split('\n');
	let name = '';
	let description = '';
	const params: Record<string, PlaybookParam> = {};

	let currentParam: string | null = null;
	let currentField: string | null = null;
	let inParams = false;

	for (const line of lines) {
		const trimmed = line.trimEnd();

		// Top-level fields
		if (!trimmed.startsWith(' ') && !trimmed.startsWith('\t')) {
			currentParam = null;
			currentField = null;
			inParams = false;

			const match = trimmed.match(/^(\w+):\s*(.*)/);
			if (match) {
				const [, key, value] = match;
				if (key === 'name') name = unquote(value);
				else if (key === 'description') description = unquote(value);
				else if (key === 'params') inParams = true;
			}
			continue;
		}

		if (!inParams) continue;

		// Param-level indent (2 spaces)
		const paramMatch = trimmed.match(/^  (\w[\w-]*):\s*(.*)/);
		if (paramMatch && !trimmed.startsWith('    ')) {
			currentParam = paramMatch[1];
			currentField = null;
			params[currentParam] = { type: 'string', description: '' };
			continue;
		}

		// Param field indent (4 spaces)
		if (currentParam) {
			const fieldMatch = trimmed.match(/^\s{4}(\w+):\s*(.*)/);
			if (fieldMatch) {
				const [, field, value] = fieldMatch;
				const param = params[currentParam];
				if (field === 'type') param.type = unquote(value);
				else if (field === 'description') param.description = unquote(value);
				else if (field === 'default') param.default = parseYamlValue(value);
				else if (field === 'enum') {
					currentField = 'enum';
					// Inline array: enum: [a, b, c]
					if (value.startsWith('[')) {
						param.enum = value
							.slice(1, -1)
							.split(',')
							.map(s => parseYamlValue(s.trim()));
					} else {
						param.enum = [];
					}
				} else {
					currentField = null;
				}
				continue;
			}

			// Enum list items (6 spaces + -)
			if (currentField === 'enum') {
				const itemMatch = trimmed.match(/^\s{6}-\s*(.*)/);
				if (itemMatch && params[currentParam].enum) {
					params[currentParam].enum!.push(parseYamlValue(itemMatch[1]));
				}
			}
		}
	}

	if (!name) throw new Error('Playbook name is empty');
	if (!description) throw new Error('Playbook description is empty');

	return { name, description, params };
}

function unquote(s: string): string {
	s = s.trim();
	if ((s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))) {
		return s.slice(1, -1);
	}
	return s;
}

function parseYamlValue(s: string): unknown {
	s = s.trim();
	if (s === 'true') return true;
	if (s === 'false') return false;
	if (s === 'null' || s === '~' || s === '') return null;
	if (/^-?\d+$/.test(s)) return parseInt(s, 10);
	if (/^-?\d+\.\d+$/.test(s)) return parseFloat(s);
	return unquote(s);
}

/**
 * Extract the first ```sh or ```bash fenced code block from markdown body.
 */
function extractScriptBlock(body: string): string {
	const lines = body.split('\n');
	let inBlock = false;
	const scriptLines: string[] = [];

	for (const line of lines) {
		if (!inBlock) {
			const trimmed = line.trim();
			if (trimmed.startsWith('```sh') || trimmed.startsWith('```bash')) {
				inBlock = true;
				continue;
			}
		} else if (line.trim().startsWith('```')) {
			return scriptLines.join('\n');
		} else {
			scriptLines.push(line);
		}
	}

	if (inBlock) throw new Error('Unclosed code block');
	throw new Error('No ```sh or ```bash code block found');
}

/**
 * Render a playbook script by substituting {{param}} placeholders with values.
 * Falls back to param defaults if no value provided.
 */
export function renderPlaybookScript(
	script: string,
	args: Record<string, unknown>,
	params: Record<string, PlaybookParam>
): string {
	let result = script;

	for (const [name, def] of Object.entries(params)) {
		const placeholder = `{{${name}}}`;
		if (!result.includes(placeholder)) continue;

		const value = args[name] ?? def.default;
		if (value === undefined || value === null) {
			throw new Error(`Missing required parameter: ${name}`);
		}

		result = result.replaceAll(placeholder, String(value));
	}

	// Check for remaining unreplaced placeholders
	const remaining = result.match(/\{\{(\w+)\}\}/);
	if (remaining) {
		throw new Error(`Script references undeclared parameter: {{${remaining[1]}}}`);
	}

	return result;
}

/**
 * Validate a playbook name: must be non-empty, only alphanumeric + hyphens + underscores.
 */
export function validatePlaybookName(name: string): boolean {
	if (!name) return false;
	return /^[a-zA-Z0-9_-]+$/.test(name);
}
