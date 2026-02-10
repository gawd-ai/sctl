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
		onToggleFileBrowser?: () => void;
		fileBrowserOpen?: boolean;
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
		onToggleFileBrowser = undefined,
		fileBrowserOpen = false
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

<div class="flex items-center px-3 py-1 bg-neutral-900 border-t border-neutral-800 text-[10px] text-neutral-500 h-7">
	<!-- Dimensions (left) -->
	{#if terminalRows > 0 && terminalCols > 0}
		<span class="tabular-nums text-neutral-600">{terminalCols}&times;{terminalRows}</span>
	{/if}

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
	</div>

	<!-- Spacer -->
	<div class="flex-1"></div>

	<!-- AI status message (shown when working) -->
	{#if aiIsWorking && aiStatusMessage}
		<span class="text-[9px] text-neutral-400 truncate max-w-48 mr-2" title={aiStatusMessage}>
			{aiStatusMessage}
		</span>
	{/if}

	<!-- File browser toggle -->
	{#if onToggleFileBrowser}
		<button
			class="flex items-center gap-1 px-1.5 py-0.5 rounded transition-colors
				{fileBrowserOpen ? 'bg-neutral-700 text-neutral-200' : 'text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800'}"
			onclick={onToggleFileBrowser}
			title="Toggle file browser"
		>
			<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
				<path stroke-linecap="round" stroke-linejoin="round" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
			</svg>
		</button>
	{/if}

	<!-- AI control button (right) -->
	<button
		class="flex items-center gap-1 px-1.5 py-0.5 rounded transition-colors {disabled ? 'text-neutral-700 cursor-default' : aiButtonClass}"
		onclick={ontoggleai}
		title={disabled ? 'Session detached' : aiTitle}
		{disabled}
	>
		<!-- Sparkle icon: filled when AI is working, outline otherwise -->
		{#if aiIsWorking}
			<svg class="w-3 h-3" viewBox="0 0 24 24" fill="currentColor">
				<path d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
			</svg>
		{:else}
			<svg class="w-3 h-3" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
				<path stroke-linecap="round" stroke-linejoin="round" d="M12 2L14.09 8.26L20 9.27L15.55 13.97L16.91 20L12 16.9L7.09 20L8.45 13.97L4 9.27L9.91 8.26L12 2Z" />
			</svg>
		{/if}
		<span class="text-[9px] font-medium">AI</span>
	</button>
</div>
