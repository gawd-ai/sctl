/** Definition of a keyboard shortcut binding. */
export interface Shortcut {
	/** Key name (e.g. `'k'`, `'Enter'`, `'Escape'`). Matched case-insensitively. */
	key: string;
	/** Require Ctrl/Cmd modifier. */
	ctrl?: boolean;
	/** Require Shift modifier. */
	shift?: boolean;
	/** Require Alt/Option modifier. */
	alt?: boolean;
	/** Function to execute when the shortcut fires. */
	action: () => void;
	/** Human-readable description shown in the shortcuts panel. */
	description: string;
	/** Optional guard — shortcut only fires when this returns true. */
	when?: () => boolean;
}

/**
 * Manages keyboard shortcuts. Register shortcuts with `register()`,
 * then call `handleKeydown()` from a global keydown listener.
 * Ignores events from input/select elements (but not xterm textareas).
 */
export class KeyboardManager {
	private shortcuts: Map<string, Shortcut> = new Map();
	private idCounter = 0;

	/** Register a shortcut. Returns an unsubscribe function. */
	register(shortcut: Shortcut): () => void {
		const id = String(++this.idCounter);
		this.shortcuts.set(id, shortcut);
		return () => {
			this.shortcuts.delete(id);
		};
	}

	/** Process a keydown event. Call this from a global event listener. */
	handleKeydown(e: KeyboardEvent): void {
		// Ignore events from form elements (but not xterm's internal textarea)
		const target = e.target as HTMLElement;
		const tag = target?.tagName;
		if (tag === 'INPUT' || tag === 'SELECT') return;
		if (tag === 'TEXTAREA' && !target.closest('.xterm')) return;

		for (const shortcut of this.shortcuts.values()) {
			if (this.matches(e, shortcut)) {
				if (shortcut.when && !shortcut.when()) continue;
				e.preventDefault();
				e.stopImmediatePropagation();
				shortcut.action();
				return;
			}
		}
	}

	/** Get all registered shortcuts (for display in a help panel). */
	getAll(): Shortcut[] {
		return Array.from(this.shortcuts.values());
	}

	private matches(e: KeyboardEvent, s: Shortcut): boolean {
		if (e.key.toLowerCase() !== s.key.toLowerCase()) return false;
		if (!!s.ctrl !== e.ctrlKey) return false;
		if (!!s.shift !== e.shiftKey) return false;
		if (!!s.alt !== e.altKey) return false;
		return true;
	}
}
