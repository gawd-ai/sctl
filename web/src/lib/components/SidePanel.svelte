<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		open?: boolean;
		width?: number;
		animate?: boolean;
		children: Snippet;
		onwidthchange?: (width: number) => void;
		onresizeend?: () => void;
	}

	let {
		open = false,
		width = 384,
		animate = false,
		children,
		onwidthchange,
		onresizeend
	}: Props = $props();

	// ── Resize state ───────────────────────────────────────────────
	const MIN_WIDTH = 240;
	const MAX_WIDTH = 800;
	let resizing = $state(false);
	let resizeCleanup: (() => void) | null = null;
	let rootEl: HTMLDivElement | undefined = $state();

	$effect(() => {
		return () => { if (resizeCleanup) resizeCleanup(); };
	});

	function handleResizeStart(e: MouseEvent) {
		e.preventDefault();
		resizing = true;
		document.body.style.userSelect = 'none';
		document.body.style.cursor = 'col-resize';

		const onMouseMove = (ev: MouseEvent) => {
			if (!rootEl) return;
			const parentRect = rootEl.parentElement?.getBoundingClientRect();
			if (!parentRect) return;
			const newWidth = parentRect.right - ev.clientX;
			const clamped = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, newWidth));
			onwidthchange?.(clamped);
		};

		const cleanup = () => {
			resizing = false;
			document.body.style.userSelect = '';
			document.body.style.cursor = '';
			window.removeEventListener('mousemove', onMouseMove);
			window.removeEventListener('mouseup', cleanup);
			resizeCleanup = null;
			onresizeend?.();
		};

		window.addEventListener('mousemove', onMouseMove);
		window.addEventListener('mouseup', cleanup);
		resizeCleanup = cleanup;
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
	bind:this={rootEl}
	class="h-full shrink-0 overflow-hidden"
	style="width: {open ? width : 0}px;"
	style:transition={!resizing && animate ? 'width 300ms ease-in-out' : 'none'}
>
	<div class="flex h-full shrink-0" style="width: {width}px;">
		<!-- Resize handle -->
		<div
			class="w-1 shrink-0 cursor-col-resize transition-colors
				{resizing ? 'bg-neutral-500' : 'bg-neutral-700 hover:bg-neutral-500'}"
			onmousedown={handleResizeStart}
			role="separator"
			aria-orientation="vertical"
		></div>

		<!-- Content area -->
		<div class="flex-1 min-w-0 bg-neutral-900 flex flex-col h-full">
			{@render children()}
		</div>
	</div>
</div>
