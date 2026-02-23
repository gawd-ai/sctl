<script lang="ts">
	import type { DeviceConnectionConfig } from '../types/widget.types';
	import type { TerminalTheme, SctlinConfig } from '../types/terminal.types';
	import TerminalContainer from '../components/TerminalContainer.svelte';

	interface Props {
		config: DeviceConnectionConfig;
		theme?: TerminalTheme;
		showTabs?: boolean;
		showControlBar?: boolean;
		class?: string;
	}

	let {
		config,
		theme = undefined,
		showTabs = true,
		showControlBar = undefined,
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
