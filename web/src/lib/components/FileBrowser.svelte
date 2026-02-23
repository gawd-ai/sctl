<script lang="ts">
	import type { SctlRestClient } from '../utils/rest-client';
	import type { DirEntry } from '../types/terminal.types';
	import FileToolbar from './FileToolbar.svelte';
	import FileTree from './FileTree.svelte';
	import FileEditor from './FileEditor.svelte';
	import FileContextMenu from './FileContextMenu.svelte';

	interface Props {
		restClient: SctlRestClient | null;
		visible?: boolean;
		readonly?: boolean;
		showHidden?: boolean;
		initialPath?: string;
		width?: number;
		animate?: boolean;
		class?: string;
		onclose?: () => void;
		onfileopen?: (path: string, content: string) => void;
		onfilesave?: (path: string) => void;
		onfiledelete?: (path: string) => void;
		onpathchange?: (path: string) => void;
		onsynccd?: (path: string) => void;
		onerror?: (error: string) => void;
		onwidthchange?: (width: number) => void;
	}

	let {
		restClient,
		visible = true,
		readonly: readonlyProp = false,
		showHidden: showHiddenProp = false,
		initialPath = '/',
		class: className = '',
		onclose,
		onfileopen,
		onfilesave,
		onfiledelete,
		onpathchange,
		onsynccd,
		onerror,
		onwidthchange,
		width: widthProp = 384,
		animate = false
	}: Props = $props();

	// ── Directory state ────────────────────────────────────────────
	let currentPath = $state(initialPath);
	let entries: DirEntry[] = $state([]);
	let dirLoading = $state(false);
	let dirError: string | null = $state(null);
	let filterText = $state('');
	let showHidden = $state(showHiddenProp);

	// ── File preview/edit state ────────────────────────────────────
	let previewPath: string | null = $state(null);
	let previewContent: string | null = $state(null);
	let previewEncoding: string | undefined = $state(undefined);
	let previewSize = $state(0);
	let previewMode: string | undefined = $state(undefined);
	let previewLoading = $state(false);
	let previewError: string | null = $state(null);
	let previewTruncated = $state(false);
	let editing = $state(false);
	let editContent = $state('');
	let savedContent: string | null = $state(null);

	// ── Archive listing state ─────────────────────────────────────
	let archiveListing: string[] | null = $state(null);

	// ── Shell sync state ──────────────────────────────────────────
	let cdSync = $state(false);

	// ── Selection & keyboard state ─────────────────────────────────
	let selectedName: string | null = $state(null);
	let selectedNames: Set<string> = $state(new Set());
	let focusedIndex = $state(-1);
	let lastClickedIndex: number | null = $state(null);

	// ── Inline create/rename state ─────────────────────────────────
	let renamingName: string | null = $state(null);
	let creatingType: 'file' | 'dir' | null = $state(null);

	// ── Context menu state ─────────────────────────────────────────
	let ctxVisible = $state(false);
	let ctxX = $state(0);
	let ctxY = $state(0);
	let ctxEntry: DirEntry | null = $state(null);

	// ── Delete confirmation state ──────────────────────────────────
	let confirmingDelete = $state(false);
	let deleteTimer: ReturnType<typeof setTimeout> | null = null;

	// ── Resize state ───────────────────────────────────────────────
	const MIN_WIDTH = 240;
	const MAX_WIDTH = 800;
	const DEFAULT_WIDTH = 384; // 24rem
	let panelWidth = $state(widthProp);
	let resizing = $state(false);

	// Sync from parent prop (e.g., another file browser was resized) — skip during our own drag
	$effect(() => { if (!resizing) panelWidth = widthProp; });

	function handleResizeStart(e: MouseEvent) {
		e.preventDefault();
		resizing = true;
		document.body.style.userSelect = 'none';
		document.body.style.cursor = 'col-resize';

		const onMouseMove = (ev: MouseEvent) => {
			if (!rootEl) return;
			// Panel is on the right, so width = right edge - mouse x
			const parentRect = rootEl.parentElement?.getBoundingClientRect();
			if (!parentRect) return;
			const newWidth = parentRect.right - ev.clientX;
			panelWidth = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, newWidth));
			onwidthchange?.(panelWidth);
		};

		const onMouseUp = () => {
			resizing = false;
			document.body.style.userSelect = '';
			document.body.style.cursor = '';
			window.removeEventListener('mousemove', onMouseMove);
			window.removeEventListener('mouseup', onMouseUp);
		};

		window.addEventListener('mousemove', onMouseMove);
		window.addEventListener('mouseup', onMouseUp);
	}

	// ── Track restClient changes ───────────────────────────────────
	let lastRestClient: typeof restClient = null;
	let lastLoadedPath: string | null = null;
	let rootEl: HTMLDivElement | undefined = $state();

	let unsaved = $derived(editing && editContent !== savedContent);

	const isDir = (e: DirEntry) => e.type === 'dir';
	const isArchive = (e: DirEntry) => /\.(zip|tar\.gz|tgz|tar\.bz2|tar\.xz|gz)$/i.test(e.name);

	// Selected entry for action bar
	let selectedEntry = $derived(
		selectedName ? entries.find((e) => e.name === selectedName) ?? null : null
	);
	let isBinary = $derived(previewEncoding === 'base64');

	// Filtered/sorted entries for keyboard nav count
	let visibleEntries = $derived(
		[...entries]
			.filter((e) => {
				if (!showHidden && e.name.startsWith('.')) return false;
				if (filterText) return e.name.toLowerCase().includes(filterText.toLowerCase());
				return true;
			})
			.sort((a, b) => {
				if (isDir(a) && !isDir(b)) return -1;
				if (!isDir(a) && isDir(b)) return 1;
				return a.name.localeCompare(b.name);
			})
	);

	// ── Effects ────────────────────────────────────────────────────

	$effect(() => {
		if (visible && restClient) {
			if (restClient !== lastRestClient) {
				lastRestClient = restClient;
				lastLoadedPath = null;
				currentPath = initialPath;
			}
			if (lastLoadedPath !== currentPath) {
				loadDir(currentPath);
			}
		}
	});

	// ── Directory operations ───────────────────────────────────────

	async function loadDir(path: string) {
		if (!restClient) return;
		dirLoading = true;
		dirError = null;
		closePreview();
		cancelCreate();
		cancelRename();
		selectedName = null;
		selectedNames = new Set();
		focusedIndex = -1;
		lastClickedIndex = null;
		try {
			entries = await restClient.listDir(path);
			currentPath = path;
			lastLoadedPath = path;
			onpathchange?.(path);
			if (cdSync) onsynccd?.(path);
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to list directory';
			dirError = msg;
			entries = [];
			onerror?.(msg);
		} finally {
			dirLoading = false;
		}
	}

	/** Reload current directory entries without resetting preview/selection state. */
	async function refreshDir() {
		if (!restClient) return;
		try {
			entries = await restClient.listDir(currentPath);
			lastLoadedPath = currentPath;
		} catch {
			// Silently fail — the dir listing is stale but preview is fine
		}
	}

	function navigate(path: string) {
		loadDir(path);
	}

	function navigateUp() {
		const parent = currentPath.replace(/\/[^/]+\/?$/, '') || '/';
		loadDir(parent);
	}

	// ── File preview ───────────────────────────────────────────────

	async function openFile(entry: DirEntry) {
		const fullPath = currentPath === '/' ? `/${entry.name}` : `${currentPath}/${entry.name}`;
		if (isDir(entry)) {
			await loadDir(fullPath);
			return;
		}
		// Symlinks might point to directories — probe with listDir
		if (entry.type === 'symlink') {
			try {
				const probeEntries = await restClient!.listDir(fullPath);
				// Success means it's a directory symlink
				entries = probeEntries;
				currentPath = fullPath;
				lastLoadedPath = fullPath;
				onpathchange?.(fullPath);
				if (cdSync) onsynccd?.(fullPath);
				closePreview();
				cancelCreate();
				cancelRename();
				selectedName = null;
				focusedIndex = -1;
				return;
			} catch {
				// Not a directory — fall through to file preview
			}
		}
		if (unsaved && previewPath) {
			if (!confirm('Discard unsaved changes?')) return;
		}
		// Large files (>1MB): request first 1MB preview
		const PREVIEW_LIMIT = 1048576;
		if (entry.size > PREVIEW_LIMIT) {
			await loadFile(fullPath, entry.size, entry.mode, { limit: PREVIEW_LIMIT });
		} else {
			await loadFile(fullPath, entry.size, entry.mode);
		}
	}

	async function loadFile(path: string, size: number, mode?: string, opts?: { offset?: number; limit?: number }) {
		if (!restClient) return;
		previewLoading = true;
		previewError = null;
		previewPath = path;
		previewContent = null;
		previewEncoding = undefined;
		previewSize = size;
		previewMode = mode;
		previewTruncated = false;
		editing = false;
		editContent = '';
		savedContent = null;
		archiveListing = null;
		try {
			const result = await restClient.readFile(path, opts);
			previewContent = result.content;
			previewEncoding = result.encoding;
			previewSize = result.size;
			previewTruncated = result.truncated ?? false;
			savedContent = result.content;
			onfileopen?.(path, result.content);
			// For archives, fetch a content listing (await to avoid hex flash)
			const lower = path.toLowerCase();
			let listCmd: string | null = null;
			if (lower.endsWith('.tar.gz') || lower.endsWith('.tgz')) {
				listCmd = `tar tzf ${shellEscape(path)}`;
			} else if (lower.endsWith('.tar.bz2')) {
				listCmd = `tar tjf ${shellEscape(path)}`;
			} else if (lower.endsWith('.tar.xz')) {
				listCmd = `tar tJf ${shellEscape(path)}`;
			} else if (lower.endsWith('.zip')) {
				listCmd = `unzip -l ${shellEscape(path)}`;
			} else if (lower.endsWith('.gz') && !lower.endsWith('.tar.gz')) {
				listCmd = `gzip -l ${shellEscape(path)}`;
			}
			if (listCmd) {
				await fetchArchiveListing(listCmd);
			}
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to read file';
			previewError = msg;
			onerror?.(msg);
		} finally {
			previewLoading = false;
		}
	}

	async function fetchArchiveListing(cmd: string) {
		if (!restClient) return;
		try {
			const result = await restClient.exec(cmd);
			if (result.exit_code === 0 && result.stdout) {
				archiveListing = result.stdout.split('\n').filter((l: string) => l.length > 0);
			}
		} catch {
			// Silently fail — hex view remains as fallback
		}
	}

	function closePreview() {
		if (unsaved && previewPath) {
			if (!confirm('Discard unsaved changes?')) return;
		}
		previewPath = null;
		previewContent = null;
		previewEncoding = undefined;
		previewMode = undefined;
		previewError = null;
		previewLoading = false;
		previewTruncated = false;
		editing = false;
		editContent = '';
		savedContent = null;
		confirmingDelete = false;
		archiveListing = null;
	}

	// ── Edit operations ────────────────────────────────────────────

	function startEdit() {
		if (readonlyProp || previewEncoding === 'base64' || previewContent === null || previewTruncated) return;
		editing = true;
		editContent = previewContent;
	}

	function discardEdit() {
		editing = false;
		editContent = savedContent ?? '';
	}

	async function saveFile(content: string) {
		if (!restClient || !previewPath) return;
		try {
			await restClient.writeFile(previewPath, content);
			previewContent = content;
			savedContent = content;
			editing = false;
			onfilesave?.(previewPath);
			// Refresh dir to update sizes without closing the preview
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to save file';
			previewError = msg;
			onerror?.(msg);
		}
	}

	async function chmodFile(mode: string) {
		if (!restClient || !previewPath) return;
		try {
			await restClient.exec(`chmod ${shellEscape(mode)} ${shellEscape(previewPath)}`);
			previewMode = mode;
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to chmod';
			onerror?.(msg);
		}
	}

	// ── Create operations ──────────────────────────────────────────

	function startCreateFile() {
		cancelRename();
		creatingType = 'file';
	}

	function startCreateFolder() {
		cancelRename();
		creatingType = 'dir';
	}

	function cancelCreate() {
		creatingType = null;
	}

	async function submitCreate(name: string) {
		if (!restClient || !name) return;
		const fullPath = currentPath === '/' ? `/${name}` : `${currentPath}/${name}`;
		try {
			if (creatingType === 'dir') {
				await restClient.exec(`mkdir -p ${shellEscape(fullPath)}`);
			} else {
				await restClient.writeFile(fullPath, '');
			}
			cancelCreate();
			await refreshDir();
			selectedName = name;
		} catch (err) {
			const msg = err instanceof Error ? err.message : `Failed to create ${creatingType}`;
			onerror?.(msg);
		}
	}

	// ── Rename operations ──────────────────────────────────────────

	function startRename(name: string) {
		cancelCreate();
		renamingName = name;
	}

	function cancelRename() {
		renamingName = null;
	}

	async function submitRename(oldName: string, newName: string) {
		if (!restClient || !oldName || !newName || oldName === newName) {
			cancelRename();
			return;
		}
		const oldPath = currentPath === '/' ? `/${oldName}` : `${currentPath}/${oldName}`;
		const newPath = currentPath === '/' ? `/${newName}` : `${currentPath}/${newName}`;
		try {
			await restClient.exec(`mv ${shellEscape(oldPath)} ${shellEscape(newPath)}`);
			cancelRename();
			// Update preview path if we renamed the previewed file
			if (previewPath === oldPath) {
				previewPath = newPath;
			}
			await refreshDir();
			selectedName = newName;
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to rename';
			onerror?.(msg);
		}
	}

	// ── Delete operations ──────────────────────────────────────────

	function requestDelete(entry?: DirEntry) {
		const target = entry ?? (selectedName ? entries.find((e) => e.name === selectedName) : null);
		if (!target) return;

		if (confirmingDelete) {
			executeDelete(target);
		} else {
			confirmingDelete = true;
			selectedName = target.name;
			if (deleteTimer) clearTimeout(deleteTimer);
			deleteTimer = setTimeout(() => { confirmingDelete = false; }, 3000);
		}
	}

	async function executeDelete(entry: DirEntry) {
		if (!restClient) return;
		const fullPath = currentPath === '/' ? `/${entry.name}` : `${currentPath}/${entry.name}`;
		try {
			if (isDir(entry)) {
				await restClient.exec(`rm -rf ${shellEscape(fullPath)}`);
			} else {
				await restClient.deleteFile(fullPath);
			}
			if (previewPath === fullPath) closePreview();
			confirmingDelete = false;
			onfiledelete?.(fullPath);
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to delete';
			onerror?.(msg);
			confirmingDelete = false;
		}
	}

	function requestBatchDelete() {
		if (confirmingDelete) {
			executeBatchDelete();
		} else {
			confirmingDelete = true;
			if (deleteTimer) clearTimeout(deleteTimer);
			deleteTimer = setTimeout(() => { confirmingDelete = false; }, 3000);
		}
	}

	async function executeBatchDelete() {
		if (!restClient) return;
		const names = [...selectedNames];
		try {
			for (const name of names) {
				const fullPath = currentPath === '/' ? `/${name}` : `${currentPath}/${name}`;
				const entry = entries.find((e) => e.name === name);
				if (entry && isDir(entry)) {
					await restClient.exec(`rm -rf ${shellEscape(fullPath)}`);
				} else {
					await restClient.deleteFile(fullPath);
				}
				if (previewPath === fullPath) closePreview();
				onfiledelete?.(fullPath);
			}
			confirmingDelete = false;
			selectedNames = new Set();
			selectedName = null;
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Failed to delete';
			onerror?.(msg);
			confirmingDelete = false;
		}
	}

	function deletePreviewFile() {
		if (!previewPath) return;
		const name = previewPath.split('/').pop();
		if (!name) return;
		const entry = entries.find((e) => e.name === name);
		if (entry) requestDelete(entry);
	}

	// ── Context menu ───────────────────────────────────────────────

	function showContextMenu(e: MouseEvent, entry: DirEntry | null) {
		ctxX = e.clientX;
		ctxY = e.clientY;
		ctxEntry = entry;
		ctxVisible = true;
	}

	function handleContextAction(action: string) {
		ctxVisible = false;
		switch (action) {
			case 'open':
				if (ctxEntry) openFile(ctxEntry);
				break;
			case 'edit':
				if (ctxEntry) {
					openFile(ctxEntry).then(() => {
						// Wait for file to load, then enter edit
						requestAnimationFrame(() => startEdit());
					});
				}
				break;
			case 'copypath': {
				if (selectedNames.size > 1) {
					const paths = [...selectedNames].map((n) =>
						currentPath === '/' ? `/${n}` : `${currentPath}/${n}`
					);
					navigator.clipboard?.writeText(paths.join('\n'));
				} else if (ctxEntry) {
					const fullPath = currentPath === '/' ? `/${ctxEntry.name}` : `${currentPath}/${ctxEntry.name}`;
					navigator.clipboard?.writeText(fullPath);
				}
				break;
			}
			case 'rename':
				if (ctxEntry) startRename(ctxEntry.name);
				break;
			case 'delete':
				if (selectedNames.size > 1) {
					executeBatchDelete();
				} else if (ctxEntry) {
					executeDelete(ctxEntry);
				}
				break;
			case 'download':
				if (ctxEntry) downloadFile(ctxEntry);
				break;
			case 'upload':
				triggerUpload();
				break;
			case 'zip':
				if (selectedNames.size > 1) zipMultiple();
				else if (ctxEntry) zipEntry(ctxEntry);
				break;
			case 'unzip':
				if (ctxEntry) unzipEntry(ctxEntry);
				break;
			case 'newfile':
				startCreateFile();
				break;
			case 'newfolder':
				startCreateFolder();
				break;
			case 'refresh':
				loadDir(currentPath);
				break;
		}
	}

	// ── Keyboard handling ──────────────────────────────────────────

	function handleKeydown(e: KeyboardEvent) {
		// Don't intercept when typing in inputs
		const tag = (e.target as HTMLElement)?.tagName;
		if (tag === 'INPUT' || tag === 'TEXTAREA') {
			// Only intercept Ctrl+S in textarea
			if (e.key === 's' && (e.ctrlKey || e.metaKey) && editing) {
				e.preventDefault();
				saveFile(editContent);
			}
			return;
		}

		switch (e.key) {
			case 'ArrowDown':
				e.preventDefault();
				focusedIndex = Math.min(focusedIndex + 1, visibleEntries.length - 1);
				if (focusedIndex >= 0) selectedName = visibleEntries[focusedIndex].name;
				break;
			case 'ArrowUp':
				e.preventDefault();
				focusedIndex = Math.max(focusedIndex - 1, 0);
				if (focusedIndex >= 0) selectedName = visibleEntries[focusedIndex].name;
				break;
			case 'Enter':
				e.preventDefault();
				if (focusedIndex >= 0 && focusedIndex < visibleEntries.length) {
					openFile(visibleEntries[focusedIndex]);
				}
				break;
			case 'Backspace':
				e.preventDefault();
				if (currentPath !== '/') navigateUp();
				break;
			case 'Escape':
				e.preventDefault();
				if (ctxVisible) {
					ctxVisible = false;
				} else if (editing) {
					discardEdit();
				} else if (previewPath) {
					closePreview();
				} else {
					selectedName = null;
					selectedNames = new Set();
					focusedIndex = -1;
				}
				break;
			case 'a':
				if (e.ctrlKey || e.metaKey) {
					e.preventDefault();
					selectedNames = new Set(visibleEntries.map((en) => en.name));
					if (visibleEntries.length > 0) selectedName = visibleEntries[0].name;
				}
				break;
			case 'F2':
				e.preventDefault();
				if (selectedName && !readonlyProp) startRename(selectedName);
				break;
			case 'Delete':
				e.preventDefault();
				if (!readonlyProp) {
					if (selectedNames.size > 1) {
						requestBatchDelete();
					} else if (selectedName) {
						const entry = entries.find((en) => en.name === selectedName);
						if (entry) requestDelete(entry);
					}
				}
				break;
		}
	}

	// ── Selection handlers ─────────────────────────────────────────

	function handleSelect(entry: DirEntry, e?: MouseEvent) {
		const idx = visibleEntries.findIndex((en) => en.name === entry.name);
		focusedIndex = idx;
		confirmingDelete = false;

		if (e?.ctrlKey || e?.metaKey) {
			// Toggle individual selection
			const next = new Set(selectedNames);
			if (next.has(entry.name)) {
				next.delete(entry.name);
				selectedName = next.size > 0 ? [...next][next.size - 1] : null;
			} else {
				next.add(entry.name);
				selectedName = entry.name;
			}
			selectedNames = next;
		} else if (e?.shiftKey && lastClickedIndex !== null) {
			// Range selection
			const start = Math.min(lastClickedIndex, idx);
			const end = Math.max(lastClickedIndex, idx);
			const next = new Set(selectedNames);
			for (let i = start; i <= end; i++) {
				next.add(visibleEntries[i].name);
			}
			selectedNames = next;
			selectedName = entry.name;
		} else {
			// Single selection
			selectedName = entry.name;
			selectedNames = new Set([entry.name]);
		}
		lastClickedIndex = idx;
	}

	function handleOpen(entry: DirEntry) {
		openFile(entry);
	}

	// ── Helpers ────────────────────────────────────────────────────

	function shellEscape(s: string): string {
		return "'" + s.replace(/'/g, "'\\''") + "'";
	}

	function formatSize(bytes: number): string {
		if (bytes < 1024) return `${bytes}B`;
		if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)}K`;
		if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)}M`;
		return `${(bytes / 1073741824).toFixed(1)}G`;
	}

	// ── Download / Upload / Zip / Unzip ────────────────────────────

	function triggerBrowserDownload(blob: Blob, filename: string) {
		const url = URL.createObjectURL(blob);
		const a = document.createElement('a');
		a.href = url;
		a.download = filename;
		document.body.appendChild(a);
		a.click();
		document.body.removeChild(a);
		URL.revokeObjectURL(url);
	}

	async function downloadFile(entry: DirEntry) {
		if (!restClient) return;
		const fullPath = currentPath === '/' ? `/${entry.name}` : `${currentPath}/${entry.name}`;
		try {
			const { blob, filename } = await restClient.downloadBlob(fullPath);
			triggerBrowserDownload(blob, filename);
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Download failed';
			onerror?.(msg);
		}
	}

	function downloadCurrentPreview() {
		if (!previewPath || previewContent === null) return;
		const filename = previewPath.split('/').pop() ?? 'download';
		let blob: Blob;
		if (previewEncoding === 'base64') {
			const binary = atob(previewContent);
			const bytes = new Uint8Array(binary.length);
			for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
			blob = new Blob([bytes], { type: 'application/octet-stream' });
		} else {
			blob = new Blob([previewContent], { type: 'text/plain' });
		}
		triggerBrowserDownload(blob, filename);
	}

	function triggerUpload() {
		if (!restClient) return;
		const input = document.createElement('input');
		input.type = 'file';
		input.multiple = true;
		input.onchange = async () => {
			if (!input.files || input.files.length === 0) return;
			try {
				await restClient!.uploadFiles(currentPath, input.files);
				await refreshDir();
			} catch (err) {
				const msg = err instanceof Error ? err.message : 'Upload failed';
				onerror?.(msg);
			}
		};
		input.click();
	}

	async function zipEntry(entry: DirEntry) {
		if (!restClient) return;
		const cwd = shellEscape(currentPath);
		const escaped = shellEscape(entry.name);
		// Use tar.gz (universally available) — fall back to zip only if tar missing
		const archiveName = shellEscape(entry.name + '.tar.gz');
		const cmd = `cd ${cwd} && tar czf ${archiveName} ${escaped}`;
		try {
			await restClient.exec(cmd);
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Archive failed';
			onerror?.(msg);
		}
	}

	async function zipMultiple() {
		if (!restClient || selectedNames.size === 0) return;
		const cwd = shellEscape(currentPath);
		const names = [...selectedNames].map(shellEscape).join(' ');
		try {
			await restClient.exec(`cd ${cwd} && tar czf selection.tar.gz ${names}`);
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Archive failed';
			onerror?.(msg);
		}
	}

	async function unzipEntry(entry: DirEntry) {
		if (!restClient) return;
		const cwd = shellEscape(currentPath);
		const escaped = shellEscape(entry.name);
		const lower = entry.name.toLowerCase();
		let cmd: string;
		if (lower.endsWith('.tar.gz') || lower.endsWith('.tgz')) {
			cmd = `cd ${cwd} && tar xzf ${escaped}`;
		} else if (lower.endsWith('.tar.bz2')) {
			cmd = `cd ${cwd} && tar xjf ${escaped}`;
		} else if (lower.endsWith('.tar.xz')) {
			cmd = `cd ${cwd} && tar xJf ${escaped}`;
		} else if (lower.endsWith('.zip')) {
			cmd = `cd ${cwd} && unzip -o ${escaped}`;
		} else if (lower.endsWith('.gz')) {
			cmd = `cd ${cwd} && gunzip -k ${escaped}`;
		} else {
			onerror?.(`Unsupported archive format: ${entry.name}`);
			return;
		}
		try {
			await restClient.exec(cmd);
			await refreshDir();
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Extract failed';
			onerror?.(msg);
		}
	}
</script>

<!-- Outer: transitions width, clips during animation -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
	bind:this={rootEl}
	class="h-full shrink-0 overflow-hidden {className}"
	style="width: {visible ? panelWidth : 0}px;"
	style:transition={!resizing && animate ? 'width 300ms ease-in-out' : 'none'}
>
	<!-- Inner: maintains full width so content doesn't reflow during slide -->
	<div class="flex h-full shrink-0" style="width: {panelWidth}px;">
		<!-- Resize handle -->
		<div
			class="w-1 shrink-0 cursor-col-resize transition-colors
				{resizing ? 'bg-neutral-500' : 'bg-neutral-700 hover:bg-neutral-500'}"
			onmousedown={handleResizeStart}
			role="separator"
			aria-orientation="vertical"
		></div>
		<!-- Panel content -->
		<div
			class="flex-1 min-w-0 bg-neutral-900 flex flex-col"
			onkeydown={handleKeydown}
			tabindex="-1"
		>
		<FileToolbar
			{currentPath}
			{filterText}
			{showHidden}
			{cdSync}
			selectionCount={selectedNames.size}
			readonly={readonlyProp}
			onnavigate={navigate}
			onfilterchange={(t) => { filterText = t; }}
			ontogglehidden={() => { showHidden = !showHidden; }}
			ontogglecdsync={() => { cdSync = !cdSync; }}
			onrefresh={() => loadDir(currentPath)}
			onnewfile={startCreateFile}
			onnewfolder={startCreateFolder}
		/>

		{#if !previewPath}
		<FileTree
			{entries}
			{currentPath}
			{selectedName}
			{selectedNames}
			{focusedIndex}
			{filterText}
			{showHidden}
			{renamingName}
			{creatingType}
			loading={dirLoading}
			error={dirError}
			readonly={readonlyProp}
			{confirmingDelete}
			onselect={handleSelect}
			onopen={handleOpen}
			onnavigate={navigate}
			oncontextmenu={showContextMenu}
			onrenamesubmit={submitRename}
			onrenamecancel={cancelRename}
			oncreatesubmit={submitCreate}
			oncreatecancel={cancelCreate}
			onretry={() => loadDir(currentPath)}
			onfocuschange={(i) => { focusedIndex = i; }}
		/>
		{/if}

		<FileEditor
			filePath={previewPath}
			content={previewContent}
			encoding={previewEncoding}
			fileSize={previewSize}
			fileMode={previewMode}
			loading={previewLoading}
			error={previewError}
			truncated={previewTruncated}
			{editing}
			{editContent}
			{unsaved}
			{archiveListing}
			readonly={readonlyProp}
			onsave={saveFile}
			onclose={closePreview}
			oneditchange={(c) => { editContent = c; }}
			onretry={() => {
				if (previewPath) {
					const name = previewPath.split('/').pop();
					const entry = entries.find((e) => e.name === name);
					loadFile(previewPath, entry?.size ?? 0, entry?.mode);
				}
			}}
			onchmod={chmodFile}
		/>

		<!-- Bottom action bar (only when actionable) -->
		{#if editing || (previewPath && !readonlyProp) || (selectedNames.size > 1 && !readonlyProp) || (selectedEntry && !readonlyProp)}
		<div class="shrink-0 flex items-center gap-1 px-2 h-7 border-t border-neutral-800 bg-neutral-900">
			{#if editing}
				<!-- Edit mode actions -->
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] bg-green-800/60 hover:bg-green-700/60 text-green-300 transition-colors"
					title="Save (Ctrl+S)"
					onclick={() => saveFile(editContent)}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M5 13l4 4L19 7" />
					</svg>
					<span>save</span>
				</button>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Discard changes"
					onclick={discardEdit}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M6 18L18 6M6 6l12 12" />
					</svg>
					<span>discard</span>
				</button>
			{:else if previewPath && !readonlyProp}
				<!-- File preview actions -->
				{#if !isBinary && !previewTruncated && previewContent !== null}
					<button
						class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Edit file"
						onclick={startEdit}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M11 5H6a2 2 0 00-2 2v11a2 2 0 002 2h11a2 2 0 002-2v-5m-1.414-9.414a2 2 0 112.828 2.828L11.828 15H9v-2.828l8.586-8.586z" />
						</svg>
						<span>edit</span>
					</button>
				{/if}
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Copy path"
					onclick={() => { if (previewPath) navigator.clipboard?.writeText(previewPath); }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
						<path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
					</svg>
					<span>copy</span>
				</button>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Download file"
					onclick={downloadCurrentPreview}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4M7 10l5 5 5-5M12 15V3" />
					</svg>
					<span>dl</span>
				</button>
				<div class="flex-1"></div>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] transition-colors
						{confirmingDelete ? 'bg-red-900/40 text-red-400' : 'text-neutral-500 hover:text-red-400 hover:bg-neutral-800'}"
					title={confirmingDelete ? 'Click to confirm' : 'Delete'}
					onclick={deletePreviewFile}
					onmouseleave={() => { if (confirmingDelete) confirmingDelete = false; }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
					</svg>
					{#if confirmingDelete}<span>del?</span>{/if}
				</button>
			{:else if selectedNames.size > 1 && !readonlyProp}
				<!-- Multi-selection actions -->
				<span class="text-[9px] text-neutral-500 tabular-nums">{selectedNames.size} selected</span>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Copy paths"
					onclick={() => {
						const paths = [...selectedNames].map((n) => currentPath === '/' ? `/${n}` : `${currentPath}/${n}`);
						navigator.clipboard?.writeText(paths.join('\n'));
					}}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
						<path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
					</svg>
					<span>copy</span>
				</button>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Archive selected as tar.gz"
					onclick={zipMultiple}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<rect x="4" y="2" width="16" height="20" rx="2" />
						<path stroke-linecap="round" d="M10 6h4M10 10h4M10 14h4" />
					</svg>
					<span>tar</span>
				</button>
				<div class="flex-1"></div>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] transition-colors
						{confirmingDelete ? 'bg-red-900/40 text-red-400' : 'text-neutral-500 hover:text-red-400 hover:bg-neutral-800'}"
					title={confirmingDelete ? 'Click to confirm' : 'Delete selected'}
					onclick={requestBatchDelete}
					onmouseleave={() => { if (confirmingDelete) confirmingDelete = false; }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
					</svg>
					<span>{confirmingDelete ? 'del?' : `del ${selectedNames.size}`}</span>
				</button>
			{:else if selectedEntry && !readonlyProp}
				<!-- Single selection actions -->
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Open"
					onclick={() => { if (selectedEntry) openFile(selectedEntry); }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						{#if selectedEntry && isDir(selectedEntry)}
							<path stroke-linecap="round" stroke-linejoin="round" d="M5 19a2 2 0 01-2-2V7a2 2 0 012-2h5l2 2h5a2 2 0 012 2v8a2 2 0 01-2 2H5z" />
						{:else}
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M14 2v6h6" />
						{/if}
					</svg>
					<span>open</span>
				</button>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Copy path"
					onclick={() => {
						if (!selectedName) return;
						const p = currentPath === '/' ? `/${selectedName}` : `${currentPath}/${selectedName}`;
						navigator.clipboard?.writeText(p);
					}}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
						<path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
					</svg>
					<span>copy</span>
				</button>
				{#if selectedEntry && !isDir(selectedEntry)}
					<button
						class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Download file"
						onclick={() => { if (selectedEntry) downloadFile(selectedEntry); }}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<path stroke-linecap="round" stroke-linejoin="round" d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4M7 10l5 5 5-5M12 15V3" />
						</svg>
						<span>dl</span>
					</button>
				{/if}
				{#if selectedEntry && !isDir(selectedEntry) && isArchive(selectedEntry)}
					<button
						class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Extract archive"
						onclick={() => { if (selectedEntry) unzipEntry(selectedEntry); }}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="4" y="2" width="16" height="20" rx="2" />
							<path stroke-linecap="round" d="M10 6h4M10 10h4" />
							<path stroke-linecap="round" stroke-linejoin="round" d="M9 17l3-3 3 3" />
						</svg>
						<span>extract</span>
					</button>
				{:else}
					<button
						class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
						title="Create tar.gz archive"
						onclick={() => { if (selectedEntry) zipEntry(selectedEntry); }}
					>
						<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
							<rect x="4" y="2" width="16" height="20" rx="2" />
							<path stroke-linecap="round" d="M10 6h4M10 10h4M10 14h4" />
						</svg>
						<span>tar</span>
					</button>
				{/if}
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-neutral-500 hover:text-neutral-300 hover:bg-neutral-800 transition-colors"
					title="Rename (F2)"
					onclick={() => { if (selectedName) startRename(selectedName); }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M11 5H6a2 2 0 00-2 2v11a2 2 0 002 2h11a2 2 0 002-2v-5m-1.414-9.414a2 2 0 112.828 2.828L11.828 15H9v-2.828l8.586-8.586z" />
					</svg>
				</button>
				<div class="flex-1"></div>
				<button
					class="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] transition-colors
						{confirmingDelete ? 'bg-red-900/40 text-red-400' : 'text-neutral-500 hover:text-red-400 hover:bg-neutral-800'}"
					title={confirmingDelete ? 'Click to confirm' : 'Delete'}
					onclick={() => { if (selectedEntry) requestDelete(selectedEntry); }}
					onmouseleave={() => { if (confirmingDelete) confirmingDelete = false; }}
				>
					<svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
					</svg>
					{#if confirmingDelete}<span>del?</span>{/if}
				</button>
			{/if}
		</div>
		{/if}

		<FileContextMenu
			visible={ctxVisible}
			x={ctxX}
			y={ctxY}
			entry={ctxEntry}
			selectionCount={selectedNames.size}
			readonly={readonlyProp}
			onaction={handleContextAction}
			onclose={() => { ctxVisible = false; }}
		/>
		</div>
	</div>
</div>
