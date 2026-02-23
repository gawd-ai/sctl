<script lang="ts">
	interface Props {
		filePath: string | null;
		content: string | null;
		encoding?: string;
		fileSize: number;
		fileMode?: string;
		loading: boolean;
		error: string | null;
		truncated?: boolean;
		editing: boolean;
		editContent: string;
		unsaved: boolean;
		readonly: boolean;
		archiveListing?: string[] | null;
		onsave?: (content: string) => void;
		onclose?: () => void;
		oneditchange?: (content: string) => void;
		onretry?: () => void;
		onchmod?: (mode: string) => void;
	}

	let {
		filePath,
		content,
		encoding,
		fileSize,
		fileMode,
		loading,
		error,
		truncated = false,
		editing,
		editContent,
		unsaved,
		readonly,
		archiveListing = null,
		onsave,
		onclose,
		oneditchange,
		onretry,
		onchmod
	}: Props = $props();

	let editingMode = $state(false);
	let modeInput = $state('');
	let wordWrap = $state(true);

	let textareaEl: HTMLTextAreaElement | undefined = $state();

	let fileName = $derived(filePath?.split('/').pop() ?? '');
	let isBinary = $derived(encoding === 'base64');

	const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'ico', 'bmp', 'avif']);
	const MIME_MAP: Record<string, string> = {
		png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg',
		gif: 'image/gif', svg: 'image/svg+xml', webp: 'image/webp',
		ico: 'image/x-icon', bmp: 'image/bmp', avif: 'image/avif'
	};

	let fileExt = $derived(fileName.split('.').pop()?.toLowerCase() ?? '');
	let isImage = $derived(IMAGE_EXTS.has(fileExt));
	let imageSrc = $derived.by(() => {
		if (!isImage || !content) return null;
		if (fileExt === 'svg' && !isBinary) {
			// SVG can be text — render as blob URL
			return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(content)}`;
		}
		if (isBinary) {
			const mime = MIME_MAP[fileExt] ?? 'image/png';
			return `data:${mime};base64,${content}`;
		}
		return null;
	});

	// Hex dump for binary files (non-image)
	const BYTES_PER_LINE = 16;
	let hexLines = $derived.by(() => {
		if (!isBinary || !content || isImage) return [];
		const raw = Uint8Array.from(atob(content), c => c.charCodeAt(0));
		const lines: { offset: string; hex: string; ascii: string }[] = [];
		for (let i = 0; i < raw.length; i += BYTES_PER_LINE) {
			const chunk = raw.slice(i, i + BYTES_PER_LINE);
			const offset = i.toString(16).padStart(8, '0');
			const hexParts: string[] = [];
			const asciiParts: string[] = [];
			for (let j = 0; j < BYTES_PER_LINE; j++) {
				if (j < chunk.length) {
					hexParts.push(chunk[j].toString(16).padStart(2, '0'));
					asciiParts.push(chunk[j] >= 0x20 && chunk[j] <= 0x7e ? String.fromCharCode(chunk[j]) : '.');
				} else {
					hexParts.push('  ');
					asciiParts.push(' ');
				}
			}
			const hex = hexParts.slice(0, 8).join(' ') + '  ' + hexParts.slice(8).join(' ');
			lines.push({ offset, hex, ascii: asciiParts.join('') });
		}
		return lines;
	});

	// Auto-focus textarea on edit
	$effect(() => {
		if (editing && textareaEl) {
			textareaEl.focus();
		}
	});

	function handleKeydown(e: KeyboardEvent) {
		if (e.key === 's' && (e.ctrlKey || e.metaKey) && editing) {
			e.preventDefault();
			onsave?.(editContent);
		}
	}

	function formatSize(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	let gutterEl: HTMLDivElement | undefined = $state();

	// Split content into lines for view mode (line-aligned rendering)
	let contentLines = $derived(
		content ? content.split('\n') : []
	);

	let editLineCount = $derived(
		editing ? editContent.split('\n').length : 0
	);

	function syncGutterScroll() {
		if (gutterEl && textareaEl) {
			gutterEl.scrollTop = textareaEl.scrollTop;
		}
	}
</script>

{#if filePath}
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div class="border-t border-neutral-700 flex flex-col min-h-0 flex-1" onkeydown={handleKeydown}>
		<!-- Editor header -->
		<div class="flex items-center gap-1 px-2 h-6 bg-neutral-800/50 shrink-0">
			<!-- Unsaved dot + filename + metadata -->
			{#if unsaved}
				<span class="w-1.5 h-1.5 rounded-full bg-amber-400 shrink-0" title="Unsaved changes"></span>
			{/if}
			{#if editing}
				<span class="text-[9px] text-amber-400/60 shrink-0">editing</span>
			{/if}
			<span class="text-[10px] font-mono text-neutral-400 truncate">{fileName}</span>
			<span class="text-[9px] text-neutral-600 tabular-nums shrink-0">{formatSize(fileSize)}</span>
			{#if fileMode}
				{#if editingMode}
					<input
						type="text"
						bind:value={modeInput}
						onkeydown={(e) => {
							if (e.key === 'Enter') { e.preventDefault(); onchmod?.(modeInput); editingMode = false; }
							if (e.key === 'Escape') { e.preventDefault(); editingMode = false; }
						}}
						onblur={() => { editingMode = false; }}
						class="w-12 px-1 py-0 bg-neutral-900 border border-neutral-600 rounded text-[9px] text-neutral-300 font-mono
							tabular-nums focus:outline-none focus:border-neutral-400"
					/>
				{:else}
					<button
						class="text-[9px] text-neutral-700 tabular-nums font-mono shrink-0 hover:text-neutral-400 transition-colors"
						title={readonly ? `Permissions: ${fileMode}` : 'Click to chmod'}
						onclick={() => { if (!readonly) { modeInput = fileMode ?? ''; editingMode = true; } }}
					>{fileMode}</button>
				{/if}
			{/if}
			<div class="flex-1"></div>
			<!-- Word wrap toggle -->
			<button
				class="w-4 h-4 flex items-center justify-center rounded transition-colors shrink-0
					{wordWrap ? 'text-neutral-300' : 'text-neutral-600 hover:text-neutral-300'}"
				title={wordWrap ? 'Word wrap ON' : 'Word wrap OFF'}
				onclick={() => { wordWrap = !wordWrap; }}
			>
				<svg class="w-2.5 h-2.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M3 6h18M3 12h15a3 3 0 010 6H9m0 0l3-3m-3 3l3 3" />
				</svg>
			</button>
			<!-- Close preview -->
			<button
				class="w-4 h-4 flex items-center justify-center rounded text-neutral-500 hover:text-neutral-200 transition-colors shrink-0"
				title="Close preview"
				onclick={onclose}
			>
				<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2.5" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
				</svg>
			</button>
		</div>

		<!-- Truncation banner -->
		{#if truncated}
			<div class="flex items-center gap-1 px-2 py-1 bg-amber-900/20 border-b border-amber-800/30 shrink-0">
				<svg class="w-3 h-3 text-amber-500 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
					<circle cx="12" cy="12" r="10" />
					<line x1="12" y1="16" x2="12" y2="12" />
					<line x1="12" y1="8" x2="12.01" y2="8" />
				</svg>
				<span class="text-[9px] text-amber-400">
					Showing first {formatSize(isBinary && content ? Math.floor(content.length * 3 / 4) : (content?.length ?? 0))} of {formatSize(fileSize)}
				</span>
			</div>
		{/if}

		<!-- Content area -->
		<div class="flex-1 overflow-auto min-h-0">
			{#if loading}
				<div class="flex flex-col gap-1 p-2">
					{#each Array(12) as _}
						<div class="h-3 bg-neutral-800 rounded animate-pulse" style="width: {30 + Math.random() * 200}px"></div>
					{/each}
				</div>
			{:else if error}
				<div class="flex flex-col items-center justify-center py-8 gap-2">
					<span class="text-[10px] text-red-400">{error}</span>
					{#if onretry}
						<button
							class="px-2 py-0.5 rounded text-[10px] text-neutral-400 bg-neutral-800 hover:bg-neutral-700 hover:text-neutral-200 transition-colors"
							onclick={onretry}
						>retry</button>
					{/if}
				</div>
			{:else if isImage && imageSrc}
				<div class="flex flex-col items-center justify-center p-4 gap-2 h-full">
					<img
						src={imageSrc}
						alt={fileName}
						class="max-w-full max-h-full object-contain rounded border border-neutral-800"
					/>
					<span class="text-[9px] text-neutral-600 tabular-nums">{formatSize(fileSize)}</span>
				</div>
			{:else if isBinary && archiveListing && archiveListing.length > 0}
				<!-- Archive content listing -->
				<div class="flex-1 overflow-auto min-h-0 font-mono text-[10px] leading-relaxed">
					<div class="flex items-center gap-1.5 px-2 py-1 bg-neutral-800/30 border-b border-neutral-800 sticky top-0">
						<svg class="w-3 h-3 text-neutral-500 shrink-0" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="4" y="2" width="16" height="20" rx="2" />
							<path stroke-linecap="round" d="M10 6h4M10 10h4M10 14h4" />
						</svg>
						<span class="text-[9px] text-neutral-500">{archiveListing.length} entries</span>
					</div>
					{#each archiveListing as line}
						<div class="px-2 py-px hover:bg-neutral-800/30 text-neutral-400 whitespace-pre">{line}</div>
					{/each}
				</div>
			{:else if isBinary}
				<div class="flex-1 overflow-auto min-h-0 font-mono text-[10px] leading-relaxed">
					{#each hexLines as line}
						<div class="flex px-2 hover:bg-neutral-800/30">
							<span class="text-neutral-600 select-none w-[7ch] shrink-0">{line.offset}</span>
							<span class="text-neutral-400 w-[49ch] shrink-0 whitespace-pre">{line.hex}</span>
							<span class="text-neutral-500 shrink-0">|{line.ascii}|</span>
						</div>
					{/each}
					{#if truncated}
						<div class="flex px-2 py-1 text-neutral-600">
							<span class="w-[7ch] shrink-0">...</span>
							<span>truncated at {formatSize(isBinary && content ? Math.floor(content.length * 3 / 4) : (content?.length ?? 0))}</span>
						</div>
					{/if}
				</div>
			{:else if editing}
				<div class="flex min-h-full h-full">
					<!-- Edit mode: gutter with fixed-height lines + no-wrap textarea -->
					<div
						bind:this={gutterEl}
						class="shrink-0 px-1 pt-2 pb-2 text-right select-none border-r border-neutral-800/50 overflow-hidden"
					>
						{#each Array(editLineCount) as _, i}
							<div class="text-[9px] text-neutral-700 font-mono leading-[18px] h-[18px]">{i + 1}</div>
						{/each}
					</div>
					<textarea
						bind:this={textareaEl}
						value={editContent}
						oninput={(e) => oneditchange?.(e.currentTarget.value)}
						onscroll={syncGutterScroll}
						class="flex-1 min-w-0 bg-transparent text-[11px] font-mono text-neutral-300 py-2 px-2 resize-none focus:outline-none
							selection:bg-blue-500/30 leading-[18px] whitespace-pre overflow-auto"
						spellcheck="false"
					></textarea>
				</div>
			{:else if content !== null}
				<!-- View mode: per-line rows so line numbers align with wrapped text -->
				<div class="flex-1 overflow-auto min-h-0">
					<div class="py-2">
						{#each contentLines as line, i}
							<div class="flex hover:bg-neutral-800/20">
								<span class="shrink-0 px-1 text-right text-[9px] text-neutral-700 font-mono select-none leading-relaxed w-[3.5ch] min-w-[3.5ch]">{i + 1}</span>
								<pre class="flex-1 min-w-0 text-[11px] font-mono text-neutral-300 px-2 leading-relaxed
									{wordWrap ? 'whitespace-pre-wrap break-all' : 'whitespace-pre'}">{line}</pre>
							</div>
						{/each}
					</div>
				</div>
			{/if}
		</div>
	</div>
{/if}
