<script lang="ts">
	import { onMount } from 'svelte';
	import type { TerminalTheme } from '../types/terminal.types';
	import type { XtermInstance } from '../utils/xterm';

	interface Props {
		theme?: TerminalTheme;
		readonly?: boolean;
		overlayLabel?: string;
		overlayColor?: 'blue' | 'green' | 'gray';
		rows?: number;
		cols?: number;
		ondata?: (data: string) => void;
		onresize?: (rows: number, cols: number) => void;
		onready?: () => void;
	}

	let {
		theme = undefined,
		readonly = false,
		overlayLabel = undefined,
		overlayColor = 'green',
		rows = undefined,
		cols = undefined,
		ondata = undefined,
		onresize = undefined,
		onready = undefined
	}: Props = $props();

	let container: HTMLDivElement | undefined = $state();
	let instance: XtermInstance | null = $state(null);
	let error: string | null = $state(null);

	// Writes that arrive before xterm loads — flushed once instance is set
	let earlyWrites: string[] = [];

	// Mutable callback refs — $effect keeps them in sync with props.
	// Avoids relying on Svelte 5 prop getter semantics inside xterm closures.
	let _ondata: ((data: string) => void) | undefined;
	let _onresize: ((rows: number, cols: number) => void) | undefined;
	$effect(() => { _ondata = ondata; });
	$effect(() => { _onresize = onresize; });

	// Timers
	let fitTimer: ReturnType<typeof setTimeout> | undefined;

	onMount(() => {
		let disposed = false;
		let resizeObserver: ResizeObserver | undefined;

		(async () => {
			try {
				const { createTerminal } = await import('../utils/xterm');
				if (disposed) return;

				const inst = await createTerminal(container!, theme, { rows, cols });
				if (disposed) {
					inst.dispose();
					return;
				}
				instance = inst;

				// Flush any writes that arrived while xterm was loading
				for (const data of earlyWrites) {
					inst.terminal.write(data);
				}
				earlyWrites = [];

				// Signal that xterm is ready to receive writes
				onready?.();

				// Forward user input
				inst.terminal.onData((data) => {
					_ondata?.(data);
				});

				// Forward resize events to server immediately — the fit() call is
				// already debounced (200ms), so this won't flood SIGWINCH.  Sending
				// immediately keeps the PTY dimensions in sync with xterm and avoids
				// zsh prompt decoration artifacts during resize.
				inst.terminal.onResize(({ rows: r, cols: c }) => {
					_onresize?.(r, c);
				});

				// Note: we intentionally do NOT set disableStdin here.
				// Input gating is handled by the parent's ondata callback.
				// This avoids xterm state bugs when toggling disableStdin.

				// Fit on container resize. First callback (fires on observe)
				// is immediate to correct the initial fit if layout settled
				// after createTerminal. Subsequent resizes are debounced to
				// prevent garbling complex prompts (e.g. powerlevel10k).
				let initialFit = true;
				resizeObserver = new ResizeObserver(() => {
					if (initialFit) {
						initialFit = false;
						inst.fitAddon.fit();
					} else {
						clearTimeout(fitTimer);
						fitTimer = setTimeout(() => {
							inst.fitAddon.fit();
						}, 200);
					}
				})
				resizeObserver.observe(container!);

				// Focus
				inst.terminal.focus();
			} catch (err) {
				error = err instanceof Error ? err.message : 'Failed to initialize terminal';
				console.error('[sctlin] Terminal init error:', err);
			}
		})();

		return () => {
			disposed = true;
			clearTimeout(fitTimer);
			resizeObserver?.disconnect();
			instance?.dispose();
			instance = null;
		};
	});

	// React to readonly changes (visual only — input gating is in parent)
	$effect(() => {
		if (instance) {
			instance.terminal.options.cursorBlink = !readonly;
			instance.terminal.options.cursorStyle = readonly ? 'bar' : 'block';
			instance.terminal.options.cursorInactiveStyle = readonly ? 'none' : 'outline';
		}
	});

	// React to theme changes
	$effect(() => {
		if (instance && theme) {
			import('../utils/xterm').then(({ applyTheme: applyThemeFn }) => {
				if (instance) applyThemeFn(instance.terminal, theme!);
			});
		}
	});

	/** Write data to the terminal. Buffers if xterm hasn't loaded yet. */
	export function write(data: string): void {
		if (instance) {
			instance.terminal.write(data);
		} else {
			earlyWrites.push(data);
		}
	}

	/** Clear the terminal screen. */
	export function clear(): void {
		instance?.terminal.clear();
	}

	/** Get current terminal size (rows/cols). */
	export function getSize(): { rows: number; cols: number } | null {
		if (!instance) return null;
		return { rows: instance.terminal.rows, cols: instance.terminal.cols };
	}

	/** Re-fit the terminal to its container. */
	export function fit(): void {
		instance?.fitAddon.fit();
	}

	/** Focus the terminal. */
	export function focus(): void {
		instance?.terminal.focus();
	}
</script>

{#if error}
	<div class="sctlin-terminal-error">
		<p>Terminal failed to initialize</p>
		<pre>{error}</pre>
	</div>
{:else}
	<div class="sctlin-terminal-wrapper" class:sctlin-readonly={overlayLabel && instance}>
		<div class="sctlin-terminal" bind:this={container}></div>
		{#if overlayLabel && instance}
			<div
				class="sctlin-readonly-overlay"
				style:border-color={overlayColor === 'gray' ? '#525252' : overlayColor === 'blue' ? '#3b82f6' : '#22c55e'}
			>
				<span
					class="sctlin-readonly-badge"
					style:background={overlayColor === 'gray' ? '#525252' : overlayColor === 'blue' ? '#3b82f6' : '#22c55e'}
				>{overlayLabel}</span>
			</div>
		{/if}
	</div>
{/if}

<style>
	.sctlin-terminal-wrapper {
		position: relative;
		width: 100%;
		height: 100%;
	}

	.sctlin-terminal {
		width: 100%;
		height: 100%;
		overflow: hidden;
		padding: 8px;
		box-sizing: border-box;
	}

	.sctlin-terminal :global(.xterm) {
		height: 100%;
	}

	.sctlin-terminal-error {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		height: 100%;
		color: #ef4444;
		font-size: 0.875rem;
		gap: 0.5rem;
	}

	.sctlin-terminal-error pre {
		color: #a3a3a3;
		font-size: 0.75rem;
		max-width: 80%;
		overflow-x: auto;
	}

	.sctlin-readonly-overlay {
		position: absolute;
		inset: 0;
		pointer-events: none;
		border: 2px solid;
		border-radius: 2px;
	}

	.sctlin-readonly-badge {
		position: absolute;
		top: 4px;
		right: 8px;
		color: #000;
		font-size: 10px;
		font-weight: 600;
		padding: 2px 6px;
		border-radius: 3px;
		line-height: 1;
	}
</style>
