<script lang="ts">
	interface Props {
		userAllowsAi: boolean;
		aiIsWorking: boolean;
		aiActivity?: string;
		aiStatusMessage?: string;
		disabled?: boolean;
		terminalRows?: number;
		terminalCols?: number;
		ontoggleai?: () => void;
		onsignal?: (signal: number) => void;
		onToggleFiles?: () => void;
		onTogglePlaybooks?: () => void;
		sidePanelOpen?: boolean;
		sidePanelTab?: string;
		splitDirection?: 'horizontal' | 'vertical' | null;
		onsplithorizontal?: () => void;
		onsplitvertical?: () => void;
	}

	let {
		userAllowsAi,
		aiIsWorking = false,
		aiActivity = undefined,
		aiStatusMessage = undefined,
		disabled = false,
		terminalRows = 0,
		terminalCols = 0,
		ontoggleai = undefined,
		onsignal = undefined,
		onToggleFiles = undefined,
		onTogglePlaybooks = undefined,
		sidePanelOpen = false,
		sidePanelTab = '',
		splitDirection = null,
		onsplithorizontal = undefined,
		onsplitvertical = undefined
	}: Props = $props();

	// Four visual states:
	// 1. Dim: !userAllowsAi — user hasn't enabled AI
	// 2. Amber: userAllowsAi && !aiIsWorking — AI permitted, standing by
	// 3. Blue: aiIsWorking && activity === 'read' — AI reading
	// 4. Green: aiIsWorking && activity === 'write' (or unspecified) — AI executing
	const aiButtonClass = $derived(
		!userAllowsAi
			? 'text-neutral-600 hover:bg-neutral-800 hover:text-neutral-400'
			: aiIsWorking
				? aiActivity === 'read'
					? 'bg-blue-900/50 text-blue-400 hover:bg-blue-900/70'
					: 'bg-green-900/50 text-green-400 hover:bg-green-900/70'
				: 'bg-amber-900/50 text-amber-400 hover:bg-amber-900/70'
	);

	const aiTitle = $derived(
		!userAllowsAi
			? 'AI not permitted — click to allow AI'
			: aiIsWorking
				? aiActivity === 'read'
					? 'AI is reading — click to revoke AI'
					: 'AI is executing — click to revoke AI'
				: 'AI permitted (standing by) — click to revoke AI'
	);
</script>

<div class="relative flex items-center px-1.5 py-1 bg-neutral-900 border-t border-neutral-800 text-[10px] text-neutral-500 h-7">
	<!-- Dimensions (absolutely centered) -->
	{#if terminalRows > 0 && terminalCols > 0}
		<span class="absolute inset-0 flex items-center justify-center pointer-events-none">
			<span class="tabular-nums text-neutral-600">{terminalCols}&times;{terminalRows}</span>
		</span>
	{/if}

	<!-- Split controls (left-most) -->
	<div class="flex items-center gap-0.5">
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors {splitDirection === 'vertical' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			onclick={onsplitvertical}
			title="Split vertical (Alt+\)"
		>
			<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 16 16">
				<rect x="1.5" y="1.5" width="13" height="13" rx="1.5" />
				<line x1="8" y1="1.5" x2="8" y2="14.5" />
			</svg>
		</button>
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors {splitDirection === 'horizontal' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			onclick={onsplithorizontal}
			title="Split horizontal (Alt+-)"
		>
			<svg class="w-3.5 h-3.5 ml-px" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 16 16">
				<rect x="1.5" y="1.5" width="13" height="13" rx="1.5" />
				<line x1="1.5" y1="8" x2="14.5" y2="8" />
			</svg>
		</button>
	</div>

	<span class="text-neutral-700">·</span>

	<!-- Signals -->
	<div class="flex items-center gap-0.5">
		<button
			class="px-1.5 py-0.5 rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : 'hover:bg-red-900/60 hover:text-red-300 text-neutral-500'}"
			onclick={() => onsignal?.(2)}
			title="SIGINT (Ctrl+C)"
			{disabled}
		>
			^C
		</button>
		<button
			class="px-1.5 py-0.5 rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : 'hover:bg-red-900/60 hover:text-red-300 text-neutral-500'}"
			onclick={() => onsignal?.(15)}
			title="SIGTERM"
			{disabled}
		>
			TERM
		</button>
		<button
			class="px-1.5 py-0.5 rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : 'hover:bg-red-900/60 hover:text-red-300 text-neutral-500'}"
			onclick={() => onsignal?.(9)}
			title="SIGKILL"
			{disabled}
		>
			KILL
		</button>
		<button
			class="px-1.5 py-0.5 rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : 'hover:bg-yellow-900/60 hover:text-yellow-300 text-neutral-500'}"
			onclick={() => onsignal?.(1)}
			title="SIGHUP (reload)"
			{disabled}
		>
			HUP
		</button>
	</div>

	<!-- Spacer -->
	<div class="flex-1"></div>

	<!-- AI status message (shown when working) -->
	{#if aiIsWorking && aiStatusMessage}
		<span class="text-[9px] text-neutral-400 truncate max-w-48 mr-2" title={aiStatusMessage}>
			{aiStatusMessage}
		</span>
	{/if}

	<!-- Panel & AI controls (right) -->
	<div class="flex items-center gap-0.5">
		{#if onToggleFiles}
			<button
				class="w-5 h-5 flex items-center justify-center rounded transition-colors
					{sidePanelOpen && sidePanelTab === 'files' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
				onclick={onToggleFiles}
				title="Toggle file browser (Alt+E)"
			>
				<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
				</svg>
			</button>
		{/if}
		{#if onTogglePlaybooks}
			<button
				class="w-5 h-5 flex items-center justify-center rounded transition-colors
					{sidePanelOpen && sidePanelTab === 'playbooks' ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
				onclick={onTogglePlaybooks}
				title="Toggle playbooks (Alt+B)"
			>
				<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M12 6.253v13m0-13C10.832 5.477 9.246 5 7.5 5S4.168 5.477 3 6.253v13C4.168 18.477 5.754 18 7.5 18s3.332.477 4.5 1.253m0-13C13.168 5.477 14.754 5 16.5 5c1.747 0 3.332.477 4.5 1.253v13C19.832 18.477 18.247 18 16.5 18c-1.746 0-3.332.477-4.5 1.253" />
				</svg>
			</button>
		{/if}
		<button
			class="w-5 h-5 flex items-center justify-center rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : aiButtonClass}"
			onclick={ontoggleai}
			title={disabled ? 'Session detached' : aiTitle}
			{disabled}
		>
			{#if aiIsWorking}
				<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="currentColor">
					<path d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
				</svg>
			{:else}
				<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
					<path stroke-linecap="round" stroke-linejoin="round" d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
				</svg>
			{/if}
		</button>
	</div>
</div>
