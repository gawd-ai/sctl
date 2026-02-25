<script lang="ts">
	import type { Shortcut } from '../utils/keyboard';

	interface Props {
		shortcuts: Shortcut[];
		expandedWidth?: number;
	}

	let { shortcuts, expandedWidth = 256 }: Props = $props();

	interface ShortcutEntry {
		key: string;
		description: string;
	}

	interface Section {
		title: string;
		icon: string;
		entries: ShortcutEntry[];
	}

	function formatKey(s: Shortcut): string {
		const parts: string[] = [];
		if (s.ctrl) parts.push('ctrl');
		if (s.alt) parts.push('alt');
		if (s.shift) parts.push('shift');
		parts.push(s.key.length === 1 ? s.key.toUpperCase() : s.key);
		return parts.join('+');
	}

	let sections = $derived.by(() => {
		const find = (desc: string) => {
			const s = shortcuts.find(s => s.description === desc);
			return s ? formatKey(s) : '';
		};

		const sects: Section[] = [
			{
				title: 'sessions & tabs',
				icon: '>>',
				entries: [
					{ key: find('New session on active server'), description: 'new session' },
					{ key: find('Close active tab'), description: 'close tab' },
					{ key: find('Previous tab'), description: 'prev tab' },
					{ key: find('Next tab'), description: 'next tab' },
					{ key: 'alt+1..9', description: 'jump to tab n' },
				]
			},
			{
				title: 'terminal',
				icon: '$_',
				entries: [
					{ key: find('Toggle terminal search'), description: 'search' },
					{ key: find('Toggle quick exec bar'), description: 'quick exec' },
					{ key: find('Split terminal vertically'), description: 'split vertical' },
					{ key: find('Split terminal horizontally'), description: 'split horizontal' },
					{ key: find('Close split pane'), description: 'close split' },
					{ key: find('Toggle split focus'), description: 'toggle split focus' },
				]
			},
			{
				title: 'panels',
				icon: '[]',
				entries: [
					{ key: find('Toggle command palette'), description: 'command palette' },
					{ key: find('Toggle file browser'), description: 'file browser' },
					{ key: find('Toggle playbook panel'), description: 'playbooks' },
					{ key: find('Server dashboard'), description: 'dashboard' },
					{ key: find('Keyboard shortcuts'), description: 'this panel' },
				]
			},
		];

		return sects;
	});
</script>

<!-- Fixed min-width prevents text reflow during sidebar expand animation -->
<div class="font-mono" style="min-width: {expandedWidth}px">
	<!-- Header -->
	<div class="flex items-center gap-2 px-3 py-2.5">
		<svg class="w-3.5 h-3.5 text-green-500/70 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
			<circle cx="12" cy="12" r="10" />
			<path stroke-linecap="round" d="M9.09 9a3 3 0 015.83 1c0 2-3 3-3 3M12 17h.01" />
		</svg>
		<span class="text-[10px] text-neutral-500 uppercase tracking-[0.15em]">keyboard shortcuts</span>
	</div>

	<!-- Sections -->
	<div class="px-3 pb-3 space-y-3">
		{#each sections as section}
			<div>
				<!-- Section header: baseline-align icon + label, center the rule line -->
				<div class="flex items-baseline gap-2 mb-1">
					<span class="text-green-500/60 text-[9px] leading-none shrink-0 w-4 text-right">{section.icon}</span>
					<span class="text-[9px] leading-none text-neutral-600 uppercase tracking-[0.12em]">{section.title}</span>
					<div class="flex-1 border-t border-neutral-800/50 self-center"></div>
				</div>
				{#each section.entries as entry}
					<div class="flex items-center justify-between py-[3px] px-1 rounded hover:bg-neutral-800/40 group">
						<span class="text-[11px] text-neutral-500 group-hover:text-neutral-300 transition-colors">{entry.description}</span>
						<kbd class="text-[10px] text-green-500/70 bg-neutral-900/80 border border-neutral-800/70 px-1.5 py-0.5 rounded min-w-[54px] text-center shrink-0 ml-3">{entry.key}</kbd>
					</div>
				{/each}
			</div>
		{/each}
	</div>
</div>
