import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import FileTree from './FileTree.svelte';
import type { DirEntry } from '../types/terminal.types';

/** A file a unit may hold, named with markup. */
const NAME = '<img src=x onerror=alert(1)>';

function tree(filterText: string) {
	const entries: DirEntry[] = [{ name: NAME, type: 'file', size: 1 }];
	return render(FileTree, {
		props: {
			entries,
			currentPath: '/tmp',
			selectedName: null,
			selectedNames: new Set<string>(),
			focusedIndex: -1,
			filterText,
			showHidden: true,
			renamingName: null,
			creatingType: null,
			loading: false,
			error: null,
			readonly: true
		}
	});
}

describe('FileTree', () => {
	it('renders a name with markup as text, with its match marked', () => {
		const { container } = tree('img');
		expect(container.querySelector('img')).toBeNull();
		const marks = container.querySelectorAll('mark');
		expect(marks).toHaveLength(1);
		expect(marks[0].textContent).toBe('img');
		expect(marks[0].parentElement?.textContent?.trim()).toBe(NAME);
	});

	it('renders the name as text with no filter', () => {
		const { container } = tree('');
		expect(container.querySelector('img')).toBeNull();
		expect(container.querySelector('mark')).toBeNull();
		expect(container.textContent).toContain(NAME);
	});
});
