export interface Shortcut {
	key: string;
	ctrl?: boolean;
	shift?: boolean;
	alt?: boolean;
	action: () => void;
	description: string;
	when?: () => boolean;
}

export class KeyboardManager {
	private shortcuts: Map<string, Shortcut> = new Map();
	private idCounter = 0;

	register(shortcut: Shortcut): () => void {
		const id = String(++this.idCounter);
		this.shortcuts.set(id, shortcut);
		return () => {
			this.shortcuts.delete(id);
		};
	}

	handleKeydown(e: KeyboardEvent): void {
		// Ignore events from form elements
		const tag = (e.target as HTMLElement)?.tagName;
		if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;

		for (const shortcut of this.shortcuts.values()) {
			if (this.matches(e, shortcut)) {
				if (shortcut.when && !shortcut.when()) continue;
				e.preventDefault();
				shortcut.action();
				return;
			}
		}
	}

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
