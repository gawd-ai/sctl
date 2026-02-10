<script lang="ts">
	import type { Shortcut } from '../utils/keyboard';

	interface PaletteEntry {
		id: string;
		label: string;
		description: string;
		shortcut?: string;
		action: () => void;
	}

	interface Props {
		visible: boolean;
		shortcuts: Shortcut[];
		extraEntries?: PaletteEntry[];
		onclose?: () => void;
	}

	let { visible, shortcuts, extraEntries = [], onclose = undefined }: Props = $props();

	let query = $state('');
	let selectedIndex = $state(0);
	let inputEl: HTMLInputElement | undefined = $state();

	$effect(() => {
		if (visible) {
			query = '';
			selectedIndex = 0;
			setTimeout(() => inputEl?.focus(), 50);
		}
	});

	function formatShortcut(s: Shortcut): string {
		const parts: string[] = [];
		if (s.ctrl) parts.push('Ctrl');
		if (s.alt) parts.push('Alt');
		if (s.shift) parts.push('Shift');
		parts.push(s.key.length === 1 ? s.key.toUpperCase() : s.key);
		return parts.join('+');
	}

	let allEntries = $derived([
		...shortcuts.map((s, i) => ({
			id: `shortcut-${i}`,
			label: s.description,
			description: '',
			shortcut: formatShortcut(s),
			action: s.action
		})),
		...extraEntries
	]);

	let filtered = $derived(
		query.trim()
			? allEntries.filter((e) => {
					const q = query.toLowerCase();
					return e.label.toLowerCase().includes(q) ||
						e.description.toLowerCase().includes(q) ||
						(e.shortcut?.toLowerCase().includes(q) ?? false);
				})
			: allEntries
	);

	let prevFilteredLen = 0;
	$effect(() => {
		const len = filtered.length;
		if (len !== prevFilteredLen) {
			prevFilteredLen = len;
			selectedIndex = 0;
		}
	});

	function handleKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			onclose?.();
		} else if (e.key === 'ArrowDown') {
			e.preventDefault();
			selectedIndex = Math.min(selectedIndex + 1, filtered.length - 1);
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			selectedIndex = Math.max(selectedIndex - 1, 0);
		} else if (e.key === 'Enter') {
			e.preventDefault();
			if (filtered[selectedIndex]) {
				filtered[selectedIndex].action();
				onclose?.();
			}
		}
	}

	function handleEntryClick(idx: number) {
		if (filtered[idx]) {
			filtered[idx].action();
			onclose?.();
		}
	}

	function handleBackdropClick(e: MouseEvent) {
		if (e.target === e.currentTarget) onclose?.();
	}
</script>

{#if visible}
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<div class="fixed inset-0 z-50 bg-black/30" onclick={handleBackdropClick}>
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div
			class="fixed top-[15%] left-1/2 -translate-x-1/2 w-96 max-w-[90vw] bg-neutral-900 border border-neutral-700 rounded-lg shadow-2xl overflow-hidden"
			onkeydown={handleKeydown}
		>
			<!-- Search input -->
			<div class="px-3 py-2 border-b border-neutral-800">
				<input
					bind:this={inputEl}
					bind:value={query}
					placeholder="Type a command..."
					class="w-full bg-transparent text-sm font-mono text-neutral-200 placeholder:text-neutral-600 focus:outline-none"
				/>
			</div>

			<!-- Results -->
			<div class="max-h-64 overflow-y-auto">
				{#if filtered.length === 0}
					<div class="px-3 py-4 text-[11px] text-neutral-600 text-center">No matching commands</div>
				{:else}
					{#each filtered as entry, i (entry.id)}
						<button
							class="w-full flex items-center gap-2 px-3 py-1.5 text-left transition-colors
								{i === selectedIndex ? 'bg-neutral-800 text-neutral-200' : 'text-neutral-400 hover:bg-neutral-800/50'}"
							onclick={() => handleEntryClick(i)}
							onmouseenter={() => { selectedIndex = i; }}
						>
							<span class="text-[11px] font-mono flex-1 truncate">{entry.label}</span>
							{#if entry.shortcut}
								<span class="text-[9px] font-mono text-neutral-600 bg-neutral-800 px-1.5 py-0.5 rounded shrink-0">{entry.shortcut}</span>
							{/if}
						</button>
					{/each}
				{/if}
			</div>
		</div>
	</div>
{/if}
