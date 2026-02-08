import type { TerminalTheme } from '../types/terminal.types';

/** Default dark terminal theme — standard ANSI colors for broad compatibility. */
export const DEFAULT_THEME: TerminalTheme = {
	background: 'rgba(12, 12, 12, 0)',
	foreground: '#cccccc',
	cursor: '#ffffff',
	cursorAccent: '#0c0c0c',
	selectionBackground: '#264f78',
	selectionForeground: '#ffffff',
	black: 'rgba(12, 12, 12, 0.99)',
	red: '#c50f1f',
	green: '#13a10e',
	yellow: '#c19c00',
	blue: '#0037da',
	magenta: '#881798',
	cyan: '#3a96dd',
	white: '#cccccc',
	brightBlack: '#767676',
	brightRed: '#e74856',
	brightGreen: '#16c60c',
	brightYellow: '#f9f1a5',
	brightBlue: '#3b78ff',
	brightMagenta: '#b4009e',
	brightCyan: '#61d6d6',
	brightWhite: '#f2f2f2',
	fontFamily: "'MesloLGS NF', 'Cascadia Code', 'Consolas', monospace",
	fontSize: 14
};

/** Convert our TerminalTheme to xterm.js ITheme (strip non-ITheme fields). */
function toXtermTheme(theme: TerminalTheme): Record<string, string | undefined> {
	const { fontFamily: _, fontSize: __, ...itheme } = theme;
	return itheme;
}

export interface XtermInstance {
	terminal: import('@xterm/xterm').Terminal;
	fitAddon: import('@xterm/addon-fit').FitAddon;
	dispose: () => void;
}

/**
 * Create an xterm.js Terminal, attach FitAddon + WebLinksAddon, and open it
 * inside the given container element.
 *
 * Must be called in the browser (after mount). All xterm imports are dynamic
 * so this module is safe to reference from SSR code.
 */
export async function createTerminal(
	container: HTMLElement,
	theme?: TerminalTheme,
	options?: { rows?: number; cols?: number }
): Promise<XtermInstance> {
	const [{ Terminal }, { FitAddon }, { WebLinksAddon }] = await Promise.all([
		import('@xterm/xterm'),
		import('@xterm/addon-fit'),
		import('@xterm/addon-web-links')
	]);

	// Dynamic CSS import — xterm needs its stylesheet
	await import('@xterm/xterm/css/xterm.css');

	const resolved = theme ?? DEFAULT_THEME;
	const terminal = new Terminal({
		theme: toXtermTheme(resolved),
		fontFamily: resolved.fontFamily ?? DEFAULT_THEME.fontFamily,
		fontSize: resolved.fontSize ?? DEFAULT_THEME.fontSize,
		cursorBlink: true,
		allowProposedApi: true
	});

	const fitAddon = new FitAddon();
	const webLinksAddon = new WebLinksAddon();
	terminal.loadAddon(fitAddon);
	terminal.loadAddon(webLinksAddon);

	terminal.open(container);

	// Wait for web fonts to load so character cell measurements are accurate
	await document.fonts.ready;

	fitAddon.fit();

	return {
		terminal,
		fitAddon,
		dispose: () => {
			webLinksAddon.dispose();
			fitAddon.dispose();
			terminal.dispose();
		}
	};
}

/** Apply a theme to an existing xterm.js terminal. */
export function applyTheme(
	terminal: import('@xterm/xterm').Terminal,
	theme: TerminalTheme
): void {
	terminal.options.theme = toXtermTheme(theme);
	if (theme.fontFamily) terminal.options.fontFamily = theme.fontFamily;
	if (theme.fontSize) terminal.options.fontSize = theme.fontSize;
}
