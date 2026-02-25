<script lang="ts">
	import type { SessionInfo, SplitGroupInfo } from '../types/terminal.types';

	interface Props {
		sessions: SessionInfo[];
		activeSessionId: string | null;
		splitGroups?: SplitGroupInfo[];
		focusedPane?: 'primary' | 'secondary';
		inline?: boolean;
		onselect?: (key: string) => void;
		onclose?: (key: string) => void;
		onrename?: (key: string, label: string) => void;
		ondotclick?: (key: string) => void;
		onunsplit?: (primaryKey: string) => void;
	}

	let {
		sessions,
		activeSessionId,
		splitGroups = [],
		focusedPane = 'primary',
		inline = false,
		onselect = undefined,
		onclose = undefined,
		onrename = undefined,
		ondotclick = undefined,
		onunsplit = undefined
	}: Props = $props();

	type TabItem =
		| { type: 'single'; session: SessionInfo }
		| { type: 'group'; primary: SessionInfo; secondary: SessionInfo };

	let displayItems: TabItem[] = $derived.by(() => {
		if (splitGroups.length === 0) {
			return sessions.map(s => ({ type: 'single' as const, session: s }));
		}
		// Build lookup: secondaryKey → primaryKey
		const secondaryKeys = new Set(splitGroups.map(g => g.secondaryKey));
		const groupByPrimary = new Map(splitGroups.map(g => [g.primaryKey, g]));

		const items: TabItem[] = [];
		for (const s of sessions) {
			if (secondaryKeys.has(s.key)) continue; // skip — will be grouped with primary
			const group = groupByPrimary.get(s.key);
			if (group) {
				const secondary = sessions.find(x => x.key === group.secondaryKey);
				if (secondary) {
					items.push({ type: 'group', primary: s, secondary });
				} else {
					items.push({ type: 'single', session: s });
				}
			} else {
				items.push({ type: 'single', session: s });
			}
		}
		return items;
	});

	/** Find the group a key belongs to. */
	function groupFor(key: string): SplitGroupInfo | undefined {
		return splitGroups.find(g => g.primaryKey === key || g.secondaryKey === key);
	}

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

{#snippet dotButton(session: SessionInfo)}
	<button
		class="w-4 h-4 mr-0.5 flex items-center justify-center rounded transition-colors {session.dead ? 'cursor-default' : 'hover:bg-neutral-600/50'}"
		title={session.dead ? 'Session lost' : session.attached ? 'Detach session' : 'Attach session'}
		onclick={(e: MouseEvent) => { e.stopPropagation(); if (!session.dead) ondotclick?.(session.key); }}
		disabled={session.dead}
	>
		<span class="w-1.5 h-1.5 rounded-full shrink-0
			{session.dead ? 'bg-neutral-500' : session.attached ? 'bg-green-500' : 'bg-yellow-500'}"></span>
	</button>
{/snippet}

{#snippet closeButton(active: boolean, onclick: (e: MouseEvent) => void, title: string)}
	<div class="overflow-hidden transition-all duration-150" style="width: {active ? '16px' : '0px'}">
		<button
			class="w-4 h-4 flex items-center justify-center rounded text-neutral-400 hover:bg-neutral-600/50 hover:text-red-400"
			{onclick}
			{title}
		>
			<svg class="w-2.5 h-2.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
			</svg>
		</button>
	</div>
{/snippet}

<div role="tablist" class="flex items-center bg-neutral-900 overflow-x-auto" style:display={inline ? 'contents' : undefined}>
	{#each displayItems as item (item.type === 'single' ? item.session.key : `${item.primary.key}+${item.secondary.key}`)}
		{#if item.type === 'single'}
			{@const session = item.session}
			<div
				role="tab"
				tabindex="0"
				aria-selected={session.key === activeSessionId}
				class="group flex items-center gap-0.5 pl-1 pr-1 h-full text-[10px] leading-none transition-colors whitespace-nowrap cursor-pointer select-none
					{session.key === activeSessionId
						? 'bg-neutral-800 text-neutral-200'
						: 'text-neutral-500 hover:bg-neutral-800/50 hover:text-neutral-300'}"
				onclick={() => onselect?.(session.key)}
				onkeydown={(e) => handleTabKeydown(e, session.key)}
			>
				{@render dotButton(session)}
				{#if editingKey === session.key}
					<!-- svelte-ignore a11y_autofocus -->
					<input
						class="bg-neutral-700 text-neutral-200 text-[10px] font-mono px-1 py-0 rounded border border-neutral-500 outline-none w-24"
						bind:value={editValue}
						onblur={commitEdit}
						onkeydown={handleEditKeydown}
						onclick={(e) => e.stopPropagation()}
						autofocus
					/>
				{:else}
					<!-- svelte-ignore a11y_no_static_element_interactions -->
					<span
						class="font-mono translate-y-px {session.dead ? 'line-through text-neutral-600' : ''}"
						ondblclick={(e) => { e.stopPropagation(); if (!session.dead) startEditing(session); }}
					>{#if session.serverName}<span class="text-neutral-600">{session.serverName} · </span>{/if}{session.label || shortId(session.sessionId)}</span>
				{/if}
				{@render closeButton(
					session.key === activeSessionId,
					(e) => { e.stopPropagation(); onclose?.(session.key); },
					'Close tab'
				)}
			</div>
		{:else}
			<!-- Grouped split tab pair -->
			{@const primary = item.primary}
			{@const secondary = item.secondary}
			{@const isGroupActive = activeSessionId === primary.key}
			<div class="flex items-center gap-0.5 h-full pl-1 pr-1 transition-colors
				{isGroupActive ? 'bg-neutral-800' : 'hover:bg-neutral-800/50'}">
				<!-- Primary half -->
				<div
					role="tab"
					tabindex="0"
					aria-selected={activeSessionId === primary.key}
					class="flex items-center gap-0.5 pr-1 text-[10px] leading-none transition-colors whitespace-nowrap cursor-pointer select-none
						{isGroupActive && focusedPane === 'primary' ? 'text-neutral-100 font-medium' : isGroupActive ? 'text-neutral-500' : 'text-neutral-500 hover:text-neutral-300'}"
					onclick={() => onselect?.(primary.key)}
					onkeydown={(e) => handleTabKeydown(e, primary.key)}
				>
					{@render dotButton(primary)}
					{#if editingKey === primary.key}
						<!-- svelte-ignore a11y_autofocus -->
						<input
							class="bg-neutral-700 text-neutral-200 text-[10px] font-mono px-1 py-0 rounded border border-neutral-500 outline-none w-24"
							bind:value={editValue}
							onblur={commitEdit}
							onkeydown={handleEditKeydown}
							onclick={(e) => e.stopPropagation()}
							autofocus
						/>
					{:else}
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<span
							class="font-mono translate-y-px {primary.dead ? 'line-through text-neutral-600' : ''}"
							ondblclick={(e) => { e.stopPropagation(); if (!primary.dead) startEditing(primary); }}
						>{#if primary.serverName}<span class="text-neutral-600">{primary.serverName} · </span>{/if}{primary.label || shortId(primary.sessionId)}</span>
					{/if}
				</div>
				<!-- Secondary half -->
				<div
					role="tab"
					tabindex="0"
					aria-selected={activeSessionId === secondary.key}
					class="flex items-center gap-0.5 text-[10px] leading-none transition-colors whitespace-nowrap cursor-pointer select-none
						{isGroupActive && focusedPane === 'secondary' ? 'text-neutral-100 font-medium' : isGroupActive ? 'text-neutral-500' : 'text-neutral-500 hover:text-neutral-300'}"
					onclick={() => onselect?.(secondary.key)}
					onkeydown={(e) => handleTabKeydown(e, secondary.key)}
				>
					{@render dotButton(secondary)}
					{#if editingKey === secondary.key}
						<!-- svelte-ignore a11y_autofocus -->
						<input
							class="bg-neutral-700 text-neutral-200 text-[10px] font-mono px-1 py-0 rounded border border-neutral-500 outline-none w-24"
							bind:value={editValue}
							onblur={commitEdit}
							onkeydown={handleEditKeydown}
							onclick={(e) => e.stopPropagation()}
							autofocus
						/>
					{:else}
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<span
							class="font-mono translate-y-px {secondary.dead ? 'line-through text-neutral-600' : ''}"
							ondblclick={(e) => { e.stopPropagation(); if (!secondary.dead) startEditing(secondary); }}
						>{secondary.label || shortId(secondary.sessionId)}</span>
					{/if}
				</div>
				{@render closeButton(
					isGroupActive,
					(e) => { e.stopPropagation(); onunsplit?.(primary.key); },
					'Close split (Alt+Q)'
				)}
			</div>
		{/if}
	{/each}

</div>
