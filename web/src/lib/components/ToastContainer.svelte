<script lang="ts">
	type ToastType = 'info' | 'success' | 'warning' | 'error';

	interface Toast {
		id: number;
		message: string;
		type: ToastType;
		duration: number;
	}

	let toasts: Toast[] = $state([]);
	let nextId = 0;

	const MAX_VISIBLE = 5;

	const typeColors: Record<ToastType, string> = {
		info: 'border-neutral-500 bg-neutral-800',
		success: 'border-green-500 bg-neutral-800',
		warning: 'border-amber-500 bg-neutral-800',
		error: 'border-red-500 bg-neutral-800'
	};

	export function push(message: string, type: ToastType = 'info', duration = 4000): void {
		const id = nextId++;
		const toast: Toast = { id, message, type, duration };
		toasts = [...toasts.slice(-(MAX_VISIBLE - 1)), toast];
		if (duration > 0) {
			setTimeout(() => dismiss(id), duration);
		}
	}

	function dismiss(id: number): void {
		toasts = toasts.filter((t) => t.id !== id);
	}
</script>

{#if toasts.length > 0}
	<div class="fixed bottom-4 right-4 z-50 flex flex-col gap-1 pointer-events-none" role="log" aria-live="polite">
		{#each toasts as toast (toast.id)}
			<button
				class="pointer-events-auto max-w-80 px-3 py-1.5 rounded border-l-2 shadow-lg text-[11px] font-mono text-neutral-300 text-left transition-all animate-slide-up {typeColors[toast.type]}"
				onclick={() => dismiss(toast.id)}
			>
				{toast.message}
			</button>
		{/each}
	</div>
{/if}

<style>
	@keyframes slide-up {
		from {
			opacity: 0;
			transform: translateY(8px);
		}
		to {
			opacity: 1;
			transform: translateY(0);
		}
	}

	.animate-slide-up {
		animation: slide-up 0.2s ease-out;
	}
</style>
