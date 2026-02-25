<script lang="ts">
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { TerminalTheme, SctlinConfig } from '../types/terminal.types';
	import TerminalContainer from '../components/TerminalContainer.svelte';

	/** Self-contained terminal that manages its own WS connection and auto-starts a session. */
	interface Props {
		/** Connection details (wsUrl, apiKey). Required. */
		config: DeviceConnectionConfig;
		/** Terminal color/font theme. Uses sctlin defaults if omitted. */
		theme?: TerminalTheme;
		/** Show the session tab bar. Default: true. */
		showTabs?: boolean;
		/** Additional CSS classes on the wrapper div. */
		class?: string;
	}

	let {
		config,
		theme = undefined,
		showTabs = true,
		class: className = ''
	}: Props = $props();

	let sctlinConfig = $derived<SctlinConfig>({
		wsUrl: config.wsUrl,
		apiKey: config.apiKey,
		theme,
		autoConnect: config.autoConnect !== false,
		autoStartSession: true
	});
</script>

<div class="terminal-widget {className}">
	<TerminalContainer config={sctlinConfig} {showTabs} />
</div>
