<script lang="ts">
	import type { SessionInfo } from '../types/terminal.types';

	interface Props {
		sessions: SessionInfo[];
		activeSessionId: string | null;
		onselect?: (key: string) => void;
		onclose?: (key: string) => void;
		onrename?: (key: string, label: string) => void;
		ondotclick?: (key: string) => void;
	}

	let {
		sessions,
		activeSessionId,
		onselect = undefined,
		onclose = undefined,
		onrename = undefined,
		ondotclick = undefined
	}: Props = $props();

	let editingKey: string | null = $state(null);
	let editValue = $state('');

	function shortId(id: string): string {
		return id.length > 12 ? id.slice(0, 12) + '...' : id;
	}

	function startEditing(session: SessionInfo): void {
		editingKey = session.key;
		editValue = session.label || shortId(session.sessionId);
	}

	function commitEdit(): void {
		if (editingKey && editValue.trim()) {
			onrename?.(editingKey, editValue.trim());
		}
		editingKey = null;
	}

	function cancelEdit(): void {
		editingKey = null;
	}

	function handleTabKeydown(e: KeyboardEvent, key: string): void {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			onselect?.(key);
		}
	}

	function handleEditKeydown(e: KeyboardEvent): void {
		if (e.key === 'Enter') {
			e.preventDefault();
			commitEdit();
		} else if (e.key === 'Escape') {
			e.preventDefault();
			cancelEdit();
		}
	}
</script>

<div role="tablist" class="flex items-center bg-neutral-900 overflow-x-auto">
	{#each sessions as session (session.key)}
		<div
			role="tab"
			tabindex="0"
			aria-selected={session.key === activeSessionId}
			class="group flex items-center gap-1 px-2 py-1 text-xs border-r border-neutral-700 transition-colors whitespace-nowrap cursor-pointer select-none
				{session.key === activeSessionId
					? 'bg-neutral-800 text-neutral-200'
					: 'text-neutral-500 hover:bg-neutral-800 hover:text-neutral-300'}"
			onclick={() => onselect?.(session.key)}
			onkeydown={(e) => handleTabKeydown(e, session.key)}
		>
			<!-- Connection dot button -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded transition-colors {session.dead ? 'cursor-default' : 'hover:bg-neutral-600/50'}"
				title={session.dead ? 'Session lost' : session.attached ? 'Detach session' : 'Attach session'}
				onclick={(e: MouseEvent) => { e.stopPropagation(); if (!session.dead) ondotclick?.(session.key); }}
				disabled={session.dead}
			>
				<span class="w-1.5 h-1.5 rounded-full shrink-0
					{session.dead ? 'bg-neutral-500' : session.attached ? 'bg-green-500' : 'bg-yellow-500'}"></span>
			</button>
			{#if editingKey === session.key}
				<!-- svelte-ignore a11y_autofocus -->
				<input
					class="bg-neutral-700 text-neutral-200 text-xs font-mono px-1 py-0 rounded border border-neutral-500 outline-none w-24"
					bind:value={editValue}
					onblur={commitEdit}
					onkeydown={handleEditKeydown}
					onclick={(e) => e.stopPropagation()}
					autofocus
				/>
			{:else}
				<!-- svelte-ignore a11y_no_static_element_interactions -->
				<span
					class="font-mono {session.dead ? 'line-through text-neutral-600' : ''}"
					ondblclick={(e) => { e.stopPropagation(); if (!session.dead) startEditing(session); }}
				>{#if session.serverName}<span class="text-neutral-600">{session.serverName} · </span>{/if}{session.label || shortId(session.sessionId)}</span>
			{/if}
			<!-- Close tab button -->
			<button
				class="w-5 h-5 flex items-center justify-center rounded opacity-0 group-hover:opacity-100 transition-all text-neutral-400 hover:bg-neutral-600/50 hover:text-red-400"
				onclick={(e: MouseEvent) => { e.stopPropagation(); onclose?.(session.key); }}
				title="Close tab"
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
				</svg>
			</button>
		</div>
	{/each}

</div>
