<script lang="ts">
	import type { DirEntry } from '../types/terminal.types';

	interface MenuItem {
		label: string;
		icon?: string;
		action: string;
		danger?: boolean;
		disabled?: boolean;
		separator?: boolean;
	}

	interface Props {
		visible: boolean;
		x: number;
		y: number;
		entry: DirEntry | null;
		selectionCount: number;
		readonly: boolean;
		onaction?: (action: string) => void;
		onclose?: () => void;
	}

	let { visible, x, y, entry, selectionCount, readonly, onaction, onclose }: Props = $props();

	let menuEl: HTMLDivElement | undefined = $state();
	let focusedIdx = $state(-1);
	let confirmingDelete = $state(false);

	const isDir = (e: DirEntry | null) => e && e.type === 'dir';
	const isArchive = (e: DirEntry | null) =>
		e && /\.(zip|tar\.gz|tgz|tar\.bz2|tar\.xz|gz)$/i.test(e.name);

	let menuItems: MenuItem[] = $derived.by(() => {
		const items: MenuItem[] = [];
		const multi = selectionCount > 1;
		if (entry) {
			if (!multi) {
				if (isDir(entry)) {
					items.push({ label: 'Open', icon: 'folder', action: 'open' });
					if (!readonly) {
						items.push({ label: 'Archive', icon: 'zip', action: 'zip' });
					}
				} else {
					items.push({ label: 'Open', icon: 'file', action: 'open' });
					if (!readonly) {
						items.push({ label: 'Edit', icon: 'edit', action: 'edit' });
					}
					items.push({ label: 'Download', icon: 'download', action: 'download' });
					if (!readonly && isArchive(entry)) {
						items.push({ label: 'Extract', icon: 'unzip', action: 'unzip' });
					} else if (!readonly) {
						items.push({ label: 'Archive', icon: 'zip', action: 'zip' });
					}
				}
			} else {
				if (!readonly) {
					items.push({ label: `Archive ${selectionCount} items`, icon: 'zip', action: 'zip' });
				}
			}
			items.push({ label: multi ? `Copy ${selectionCount} paths` : 'Copy path', icon: 'copy', action: 'copypath' });
			if (!readonly) {
				if (!multi) items.push({ label: 'Rename', icon: 'rename', action: 'rename' });
				items.push({ label: multi ? `Delete ${selectionCount} items` : 'Delete', icon: 'delete', action: 'delete', danger: true });
			}
		} else {
			// Right-click on empty space
			if (!readonly) {
				items.push({ label: 'Upload file...', icon: 'upload', action: 'upload' });
				items.push({ label: 'New file', icon: 'newfile', action: 'newfile' });
				items.push({ label: 'New folder', icon: 'newfolder', action: 'newfolder' });
			}
			items.push({ label: 'Refresh', icon: 'refresh', action: 'refresh' });
		}
		return items;
	});

	// Position adjustment to keep menu on screen
	let adjustedPos = $derived.by(() => {
		let ax = x;
		let ay = y;
		// Simple bounds check (menu is roughly 160x200 max)
		if (typeof window !== 'undefined') {
			if (ax + 160 > window.innerWidth) ax = window.innerWidth - 170;
			if (ay + menuItems.length * 28 + 8 > window.innerHeight) ay = window.innerHeight - menuItems.length * 28 - 16;
			if (ax < 0) ax = 4;
			if (ay < 0) ay = 4;
		}
		return { x: ax, y: ay };
	});

	$effect(() => {
		if (visible) {
			focusedIdx = -1;
			confirmingDelete = false;
			// Click-outside to dismiss
			const handler = (e: MouseEvent) => {
				if (menuEl && !menuEl.contains(e.target as Node)) {
					onclose?.();
				}
			};
			// Defer to avoid the opening click
			requestAnimationFrame(() => {
				document.addEventListener('click', handler);
			});
			return () => document.removeEventListener('click', handler);
		}
	});

	function handleKeydown(e: KeyboardEvent) {
		if (!visible) return;
		if (e.key === 'Escape') {
			e.preventDefault();
			onclose?.();
		} else if (e.key === 'ArrowDown') {
			e.preventDefault();
			focusedIdx = Math.min(focusedIdx + 1, menuItems.length - 1);
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			focusedIdx = Math.max(focusedIdx - 1, 0);
		} else if (e.key === 'Enter' && focusedIdx >= 0) {
			e.preventDefault();
			const item = menuItems[focusedIdx];
			if (item && !item.disabled) {
				handleItemClick(item);
			}
		}
	}

	function handleItemClick(item: MenuItem) {
		if (item.disabled) return;
		if (item.action === 'delete') {
			if (confirmingDelete) {
				onaction?.(item.action);
				onclose?.();
			} else {
				confirmingDelete = true;
			}
			return;
		}
		onaction?.(item.action);
		onclose?.();
	}
</script>

{#if visible}
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		bind:this={menuEl}
		class="fixed z-[100] bg-neutral-900 border border-neutral-700 rounded-md shadow-xl py-1 min-w-[140px]"
		style="left: {adjustedPos.x}px; top: {adjustedPos.y}px;"
		onkeydown={handleKeydown}
		role="menu"
		tabindex="-1"
	>
		{#each menuItems as item, idx}
			{@const isDeleteConfirm = item.action === 'delete' && confirmingDelete}
			<button
				class="w-full flex items-center gap-2 px-3 py-1 text-[11px] font-mono transition-colors text-left
					{isDeleteConfirm ? 'bg-red-900/40 text-red-300' : ''}
					{idx === focusedIdx && !isDeleteConfirm ? 'bg-neutral-800' : ''}
					{item.danger && !isDeleteConfirm ? 'text-red-400 hover:bg-red-900/20' : ''}
					{!item.danger && !isDeleteConfirm ? 'text-neutral-300 hover:bg-neutral-800' : ''}
					{item.disabled ? 'opacity-40 cursor-not-allowed' : 'cursor-pointer'}"
				role="menuitem"
				onclick={() => handleItemClick(item)}
				onmouseenter={() => { focusedIdx = idx; }}
			>
				<!-- Icon -->
				<span class="w-3.5 h-3.5 flex items-center justify-center shrink-0 {item.danger ? 'text-red-400' : 'text-neutral-500'}">
					{#if item.icon === 'folder' || item.icon === 'open'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M5 19a2 2 0 01-2-2V7a2 2 0 012-2h5l2 2h5a2 2 0 012 2v8a2 2 0 01-2 2H5z" />
						</svg>
					{:else if item.icon === 'file'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2v6h6" />
						</svg>
					{:else if item.icon === 'edit'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M11 5H6a2 2 0 00-2 2v11a2 2 0 002 2h11a2 2 0 002-2v-5m-1.414-9.414a2 2 0 112.828 2.828L11.828 15H9v-2.828l8.586-8.586z" />
						</svg>
					{:else if item.icon === 'copy'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
							<path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
						</svg>
					{:else if item.icon === 'rename'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M13 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V9z" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M7 13h6" />
						</svg>
					{:else if item.icon === 'delete'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
						</svg>
					{:else if item.icon === 'newfile'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2v6h6M12 18v-6M9 15h6" />
						</svg>
					{:else if item.icon === 'newfolder'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M12 11v6M9 14h6" />
						</svg>
					{:else if item.icon === 'download'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4M7 10l5 5 5-5M12 15V3" />
						</svg>
					{:else if item.icon === 'upload'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4M17 8l-5-5-5 5M12 3v12" />
						</svg>
					{:else if item.icon === 'zip'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="4" y="2" width="16" height="20" rx="2" />
							<path stroke-linecap="round" d="M10 6h4M10 10h4M10 14h4" />
						</svg>
					{:else if item.icon === 'unzip'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="4" y="2" width="16" height="20" rx="2" />
							<path stroke-linecap="round" d="M10 6h4M10 10h4" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M9 17l3-3 3 3" />
						</svg>
					{:else if item.icon === 'refresh'}
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.36-5.36M20 15a9 9 0 01-15.36 5.36" />
						</svg>
					{/if}
				</span>
				<span>{isDeleteConfirm ? 'Delete?' : item.label}</span>
			</button>
		{/each}
	</div>
{/if}
