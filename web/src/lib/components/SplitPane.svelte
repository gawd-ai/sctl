<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		direction: 'horizontal' | 'vertical';
		ratio: number;
		onratiochange?: (ratio: number) => void;
		minSize?: number;
		first: Snippet;
		second: Snippet;
	}

	let {
		direction,
		ratio,
		onratiochange = undefined,
		minSize = 200,
		first,
		second
	}: Props = $props();

	let containerEl: HTMLDivElement | undefined = $state();
	let dragging = $state(false);

	function handleMouseDown(e: MouseEvent) {
		e.preventDefault();
		dragging = true;
		document.body.style.userSelect = 'none';

		const onMouseMove = (ev: MouseEvent) => {
			if (!containerEl) return;
			const rect = containerEl.getBoundingClientRect();
			let newRatio: number;

			if (direction === 'horizontal') {
				const x = ev.clientX - rect.left;
				newRatio = x / rect.width;
			} else {
				const y = ev.clientY - rect.top;
				newRatio = y / rect.height;
			}

			// Clamp to min size
			const totalSize = direction === 'horizontal' ? rect.width : rect.height;
			const minRatio = minSize / totalSize;
			const maxRatio = 1 - minRatio;
			newRatio = Math.max(minRatio, Math.min(maxRatio, newRatio));

			onratiochange?.(newRatio);
		};

		const onMouseUp = () => {
			dragging = false;
			document.body.style.userSelect = '';
			window.removeEventListener('mousemove', onMouseMove);
			window.removeEventListener('mouseup', onMouseUp);
		};

		window.addEventListener('mousemove', onMouseMove);
		window.addEventListener('mouseup', onMouseUp);
	}

	let gridTemplate = $derived(
		direction === 'horizontal'
			? `grid-template-columns: ${ratio}fr 4px ${1 - ratio}fr`
			: `grid-template-rows: ${ratio}fr 4px ${1 - ratio}fr`
	);
</script>

<div
	bind:this={containerEl}
	class="w-full h-full grid"
	style="{gridTemplate}; {direction === 'horizontal' ? 'grid-template-rows: 1fr' : 'grid-template-columns: 1fr'}"
>
	<div class="min-w-0 min-h-0 overflow-hidden">
		{@render first()}
	</div>
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		class="flex items-center justify-center transition-colors
			{direction === 'horizontal' ? 'cursor-col-resize' : 'cursor-row-resize'}
			{dragging ? 'bg-neutral-500' : 'bg-neutral-700 hover:bg-neutral-500'}"
		onmousedown={handleMouseDown}
	></div>
	<div class="min-w-0 min-h-0 overflow-hidden">
		{@render second()}
	</div>
</div>
