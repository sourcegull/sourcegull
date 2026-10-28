<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto, replaceState } from '$app/navigation';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		SYMBOL_INFO,
		type RangeInput,
		type SymbolInfo,
		type SymbolInfoResult
	} from '#lib/codenav/queries.ts';
	import UsagesPanel from '#lib/codenav/UsagesPanel.svelte';
	import OwnershipPanel from '#lib/own/OwnershipPanel.svelte';
	import { EVENTS, logEvent, viewBlobArgument } from '#lib/events.ts';
	import type { CodeNavHandlers } from '#lib/editor/codenav.ts';
	import { blameStreamURL, streamBlame, type BlameHunk } from './blame.ts';
	import CodeView from './CodeView.svelte';
	import { formatBytes } from './diff.ts';
	import { decodePacked, type Occurrence } from './highlight.ts';
	import { isMarkdownPath } from './markdown.ts';
	import MarkdownView from './MarkdownView.svelte';
	import PathBreadcrumbs from './PathBreadcrumbs.svelte';
	import { BLOB, cloneStateMessage, type BlobNode, type BlobResult } from './queries.ts';
	import {
		formatLineRange,
		parseLineRange,
		repoPageURL,
		selectLine,
		type LineRange
	} from './url.ts';

	interface Props {
		repo: string;
		urlRev: string | null;
		path: string;
	}

	let { repo, urlRev, path }: Props = $props();

	let blob = $state.raw<BlobNode | null>(null);
	let commitOid = $state<string | null>(null);
	let dirURL = $state<string | null>(null);
	let occurrences = $state.raw<Occurrence[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let selection = $state<LineRange | null>(parseLineRange(page.url.hash));
	let showBlame = $state(false);
	let showOwners = $state(false);
	let blameHunks = $state.raw<BlameHunk[] | null>(null);
	let blameError = $state<string | null>(null);
	let usagesTarget = $state.raw<{ range: RangeInput; token: string } | null>(null);
	let copied = $state(false);
	let copiedTimer: ReturnType<typeof setTimeout> | undefined;
	let seq = 0;

	const IMAGE = /\.(png|jpe?g|gif|webp|bmp|ico|avif)$/i;

	$effect(() => {
		void load(repo, urlRev ?? 'HEAD', path);
	});

	// A file view, for the recent views ownership signal.
	$effect(() => {
		logEvent('ViewBlob', viewBlobArgument(repo, path));
	});

	async function load(repoName: string, rev: string, filePath: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<BlobResult, { repo: string; rev: string; path: string }>(BLOB, {
				repo: repoName,
				rev,
				path: filePath
			});
			if (mine !== seq) return;
			const commit = r.repository?.commit ?? null;
			blob = commit?.blob ?? null;
			commitOid = commit?.oid ?? null;
			dirURL = commit?.tree?.url ?? null;
			if (!commit) error = `Revision ${rev} not found.`;
			let occ: Occurrence[] = [];
			try {
				occ = blob ? decodePacked(blob.highlight.packed) : [];
			} catch {
				occ = [];
			}
			occurrences = occ;
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	// Follow hash changes (links to other line ranges of the same file).
	$effect(() => {
		const r = parseLineRange(page.url.hash);
		if (r) selection = r;
	});

	// Code navigation: hover, go to definition, find references.
	const codeNav: CodeNavHandlers = {
		async symbolInfo(line, character) {
			const oid = commitOid;
			if (!oid) return null;
			const r = await graphql<SymbolInfoResult, Record<string, unknown>>(SYMBOL_INFO, {
				repo,
				rev: oid,
				path,
				line,
				character
			});
			return r.repository?.commit?.blob?.symbolInfo ?? null;
		},
		definitionURL(info: SymbolInfo) {
			const d = info.definition;
			if (!d) return null;
			const rev = d.repo === repo && d.commit === commitOid ? urlRev : d.commit;
			const pos = d.range ? `#L${d.range.line + 1}:${d.range.character + 1}` : '';
			return `${repoPageURL(d.repo, rev, 'blob', d.path)}${pos}`;
		},
		goTo(url) {
			logEvent('goToDefinition');
			void goto(url);
		},
		findReferences(info: SymbolInfo) {
			const oid = commitOid;
			if (!oid) return;
			logEvent('findReferences');
			usagesTarget = {
				token: blob?.content.split('\n')[info.range.start.line]?.slice(info.range.start.character, info.range.end.character) ?? '',
				range: { repository: repo, revision: oid, path, start: info.range.start, end: info.range.end }
			};
		}
	};

	// Another file closes the panel.
	$effect(() => {
		void repo;
		void path;
		usagesTarget = null;
	});

	function onlineclick(line: number, extend: boolean): void {
		selection = selectLine(selection, line, extend);
		replaceState(`#${formatLineRange(selection)}`, page.state);
	}

	// Streaming blame while the toggle is on; restarts for another file or commit.
	$effect(() => {
		const oid = commitOid;
		if (!showBlame || preview || !oid) {
			blameHunks = null;
			return;
		}
		const controller = new AbortController();
		blameHunks = [];
		blameError = null;
		const url = blameStreamURL(repo, oid, path);
		streamBlame(
			url,
			(hunks) => {
				blameHunks = [...(blameHunks ?? []), ...hunks];
			},
			controller.signal
		).catch((err: unknown) => {
			if (!controller.signal.aborted) blameError = errorMessage(err);
		});
		return () => controller.abort();
	});

	// Markdown files show the rendered preview unless `?view=code` is set or the hash selects
	// lines; `?view=preview` forces the preview.
	const markdown = $derived(isMarkdownPath(path) && blob !== null && !blob.binary);
	const preview = $derived.by(() => {
		if (!markdown) return false;
		const view = page.url.searchParams.get('view');
		if (view === 'code' || view === 'preview') return view === 'preview';
		return parseLineRange(page.url.hash) === null;
	});

	function setView(view: 'preview' | 'code'): void {
		const url = new URL(page.url.href);
		if (view === 'code') {
			url.searchParams.set('view', 'code');
		} else {
			url.searchParams.delete('view');
			if (parseLineRange(url.hash)) url.hash = '';
		}
		void goto(url, { replace: true, reset: false });
	}

	function toggleBlame(): void {
		showBlame = !showBlame;
		if (showBlame && preview) setView('code');
	}

	async function copyContents(): Promise<void> {
		if (!blob) return;
		try {
			await navigator.clipboard.writeText(blob.content);
			logEvent(EVENTS.codeCopied, { source: 'file' });
			copied = true;
			clearTimeout(copiedTimer);
			copiedTimer = setTimeout(() => (copied = false), 2000);
		} catch {
			copied = false;
		}
	}

	const permalink = $derived(
		blob ? `${blob.canonicalURL}${selection ? `#${formatLineRange(selection)}` : ''}` : null
	);
</script>

<div class="file-view">
	<div class="file-header">
		<span class="file-icon" aria-hidden="true"><Icon name="file" size={15} /></span>
		<PathBreadcrumbs {repo} {urlRev} {path} isFile={true} />
		{#if blob}
			<span class="meta">
				{#if blob.languages.length > 0}<span class="badge">{blob.languages.join(', ')}</span>{/if}
				<span class="muted">{blob.binary ? '' : `${blob.totalLines.toLocaleString()} lines · `}{formatBytes(blob.byteSize)}</span>
			</span>
			<div class="spacer"></div>
			<div class="toolbar">
				{#if markdown}
					<div class="btn-group" role="group" aria-label="View">
						<button type="button" class="btn btn-sm" aria-pressed={preview} onclick={() => setView('preview')}>Preview</button>
						<button type="button" class="btn btn-sm" aria-pressed={!preview} onclick={() => setView('code')}>Code</button>
					</div>
				{/if}
				{#if !blob.binary}
					<button
						type="button"
						class="btn btn-sm"
						aria-pressed={showBlame && !preview}
						onclick={toggleBlame}><Icon name="commit" size={14} /> {showBlame && !preview ? 'Hide blame' : 'Blame'}</button
					>
				{/if}
				<button
					type="button"
					class="btn btn-sm"
					aria-pressed={showOwners}
					onclick={() => (showOwners = !showOwners)}><Icon name="users" size={14} /> Owners</button
				>
				<a class="btn btn-sm" href={repoPageURL(repo, urlRev, 'commits', path)}><Icon name="clock" size={14} /> History</a>
				{#if !blob.binary && !blob.contentTruncated}
					<button type="button" class="btn btn-sm" title="Copy the file contents" onclick={copyContents}>{copied ? 'Copied' : 'Copy'}</button>
					<span class="visually-hidden" aria-live="polite">{copied ? 'File contents copied' : ''}</span>
				{/if}
				<a class="btn btn-sm" href={blob.rawURL} target="_blank" rel="noopener">Raw</a>
				{#if permalink}
					<a class="btn btn-sm" href={permalink} title="Link to this file at commit {commitOid}"><Icon name="link" size={14} /> Permalink</a>
				{/if}
			</div>
		{/if}
	</div>

	{#if showOwners && blob}
		<OwnershipPanel {repo} rev={commitOid ?? urlRev} {path} isDir={false} />
	{/if}

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading && !blob}
		<p class="loading pad"><span class="spinner" aria-hidden="true"></span> Loading…</p>
	{:else if !blob}
		{#if dirURL}
			<div class="alert alert-info">This path is a directory. <a href={dirURL}>Open the directory</a>.</div>
		{:else}
			<div class="alert alert-warning">File not found at this revision.</div>
		{/if}
	{:else if blob.binary}
		<div class="binary pad">
			{#if IMAGE.test(blob.name)}
				<img src={blob.rawURL} alt={blob.name} />
			{:else}
				<p class="muted">Binary file ({formatBytes(blob.byteSize)}) not shown.</p>
			{/if}
			<a class="btn" href="{blob.rawURL}?download">Download</a>
		</div>
	{:else}
		{#if blob.contentTruncated}
			<div class="alert alert-warning notice">
				This file is too large to show in full. <a href={blob.rawURL}>View the raw file</a>.
			</div>
		{:else if blob.highlight.aborted && !preview}
			<div class="alert alert-info notice">
				Syntax highlighting was skipped ({blob.highlight.plainReason ?? 'limit'}).
			</div>
		{/if}
		{#if blameError && !preview}
			<div class="alert alert-danger notice" role="alert">Blame failed: {blameError}</div>
		{/if}
		{#if preview}
			<div class="preview">
				<MarkdownView source={blob.content} {repo} rev={urlRev} {path} />
			</div>
		{:else}
			<div class="code" oncopy={() => logEvent(EVENTS.codeCopied, { source: 'selection' })}>
				<CodeView
					content={blob.content}
					{occurrences}
					{selection}
					blame={showBlame ? (blameHunks ?? []) : null}
					label={blob.path}
					{onlineclick}
					{codeNav}
				/>
			</div>
		{/if}
		{#if usagesTarget}
			<div class="panel">
				<UsagesPanel
					target={usagesTarget.range}
					token={usagesTarget.token}
					{urlRev}
					onclose={() => (usagesTarget = null)}
				/>
			</div>
		{/if}
	{/if}
</div>

<style>
	.file-view {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
	}
	.file-header {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-3);
		min-height: 2.75rem;
		padding: var(--space-2) var(--space-4);
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
	}
	.file-icon {
		display: inline-flex;
		color: var(--color-text-subtle);
		margin-right: calc(-1 * var(--space-1));
	}
	.meta {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		font-size: var(--text-xs);
	}
	.toolbar {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.toolbar :global(.icon) {
		color: var(--color-text-muted);
	}
	.spacer {
		flex: 1;
	}
	.code {
		flex: 1;
		min-height: 0;
	}
	.preview {
		flex: 1;
		min-height: 0;
		overflow: auto;
		padding: var(--space-4) var(--space-6);
	}
	.panel {
		flex: 0 0 40%;
		min-height: 0;
	}
	.pad {
		padding: var(--space-4);
	}
	.notice {
		margin: var(--space-2) var(--space-4);
	}
	.binary img {
		max-width: 100%;
		display: block;
		margin-bottom: var(--space-4);
		background: repeating-conic-gradient(var(--color-bg-muted) 0% 25%, transparent 0% 50%) 50% / 16px 16px;
	}
	.alert {
		margin: var(--space-4);
	}
</style>
