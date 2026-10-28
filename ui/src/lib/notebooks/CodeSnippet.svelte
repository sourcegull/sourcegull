<!--
	Lines of a file, highlighted on the server and with code navigation (hover, go to
	definition, find references), for file and symbol blocks. Loads through the same API as the
	file view, so repositories the viewer may not read show as not found.
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { SYMBOL_INFO, type RangeInput, type SymbolInfo, type SymbolInfoResult } from '#lib/codenav/queries.ts';
	import UsagesPanel from '#lib/codenav/UsagesPanel.svelte';
	import type { CodeNavHandlers } from '#lib/editor/codenav.ts';
	import CodeView from '#lib/repo/CodeView.svelte';
	import { decodePacked, type Occurrence } from '#lib/repo/highlight.ts';
	import { BLOB, cloneStateMessage, type BlobNode, type BlobResult } from '#lib/repo/queries.ts';
	import { formatLineRange, repoPageURL, type LineRange } from '#lib/repo/url.ts';
	import type { BlockLineRange } from './serialize.ts';

	interface Props {
		repo: string;
		/** A revision, or `''` for the default branch. */
		rev: string;
		path: string;
		/** Lines to show (0-based, end exclusive); null for the whole file. */
		lines: BlockLineRange | null;
		/** Lines to mark (1-based, inclusive). */
		selection?: LineRange | null;
	}

	let { repo, rev, path, lines, selection = null }: Props = $props();

	/** Files shown whole are cut after this many lines. */
	const MAX_LINES = 500;

	let blob = $state.raw<BlobNode | null>(null);
	let commitOid = $state<string | null>(null);
	let allOccurrences = $state.raw<Occurrence[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let usagesTarget = $state.raw<{ range: RangeInput; token: string } | null>(null);
	let seq = 0;

	$effect(() => {
		void load(repo, rev || 'HEAD', path);
	});

	async function load(repoName: string, revision: string, filePath: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		usagesTarget = null;
		try {
			const r = await graphql<BlobResult, { repo: string; rev: string; path: string }>(BLOB, {
				repo: repoName,
				rev: revision,
				path: filePath
			});
			if (mine !== seq) return;
			const commit = r.repository?.commit ?? null;
			blob = commit?.blob ?? null;
			commitOid = commit?.oid ?? null;
			if (!r.repository) error = `Repository ${repoName} not found.`;
			else if (!commit) error = `Revision ${revision} not found in ${repoName}.`;
			else if (!blob) error = `File ${filePath} not found at ${revision}.`;
			try {
				allOccurrences = blob ? decodePacked(blob.highlight.packed) : [];
			} catch {
				allOccurrences = [];
			}
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	/** The lines shown: [first, end) 0-based. */
	const shown = $derived.by(() => {
		const total = blob ? blob.content.split('\n').length : 0;
		const start = Math.max(0, Math.min(lines?.startLine ?? 0, total));
		const end = Math.min(lines ? lines.endLine : start + MAX_LINES, total);
		return { start, end: Math.max(start, end), cut: !lines && total > MAX_LINES };
	});

	const content = $derived(
		blob ? blob.content.split('\n').slice(shown.start, shown.end).join('\n') : ''
	);
	const occurrences = $derived(
		allOccurrences
			.filter((o) => o.line >= shown.start && o.line < shown.end)
			.map((o) => ({ ...o, line: o.line - shown.start }))
	);
	const docSelection = $derived(
		selection
			? { start: selection.start - shown.start, end: selection.end - shown.start }
			: null
	);
	const fileURL = $derived(
		repoPageURL(repo, rev || null, 'blob', path) +
			(lines ? `#${formatLineRange({ start: lines.startLine + 1, end: Math.max(lines.startLine + 1, lines.endLine) })}` : '')
	);

	// Code navigation: positions in the snippet are shifted to the file's.
	const codeNav: CodeNavHandlers = {
		async symbolInfo(line, character) {
			const oid = commitOid;
			if (!oid) return null;
			const offset = shown.start;
			const r = await graphql<SymbolInfoResult, Record<string, unknown>>(SYMBOL_INFO, {
				repo,
				rev: oid,
				path,
				line: line + offset,
				character
			});
			const info = r.repository?.commit?.blob?.symbolInfo ?? null;
			if (!info) return null;
			return {
				...info,
				range: {
					start: { ...info.range.start, line: info.range.start.line - offset },
					end: { ...info.range.end, line: info.range.end.line - offset }
				}
			};
		},
		definitionURL(info: SymbolInfo) {
			const d = info.definition;
			if (!d) return null;
			const pos = d.range ? `#L${d.range.line + 1}:${d.range.character + 1}` : '';
			return `${repoPageURL(d.repo, d.commit, 'blob', d.path)}${pos}`;
		},
		goTo(url) {
			void goto(url);
		},
		findReferences(info: SymbolInfo) {
			const oid = commitOid;
			if (!oid) return;
			const offset = shown.start;
			const start = { ...info.range.start, line: info.range.start.line + offset };
			const end = { ...info.range.end, line: info.range.end.line + offset };
			usagesTarget = {
				token:
					content.split('\n')[info.range.start.line]?.slice(info.range.start.character, info.range.end.character) ?? '',
				range: { repository: repo, revision: oid, path, start, end }
			};
		}
	};
</script>

<div class="snippet">
	<div class="snippet-header">
		<a class="mono path" href={fileURL}>{repo} › {path}{rev ? `@${rev}` : ''}</a>
		{#if lines}<span class="muted">lines {lines.startLine + 1}–{lines.endLine}</span>{/if}
	</div>
	{#if error}
		<div class="alert alert-warning" role="alert">{error}</div>
	{:else if loading && !blob}
		<p class="muted pad">Loading…</p>
	{:else if blob?.binary}
		<p class="muted pad">Binary file not shown.</p>
	{:else if blob}
		<div class="code">
			<CodeView
				{content}
				{occurrences}
				selection={docSelection}
				blame={null}
				label="{path}, lines {shown.start + 1} to {shown.end}"
				firstLine={shown.start + 1}
				{codeNav}
			/>
		</div>
		{#if shown.cut}
			<p class="muted pad">Showing the first {MAX_LINES} lines. <a href={fileURL}>View the whole file</a>.</p>
		{/if}
		{#if usagesTarget}
			<div class="usages">
				<UsagesPanel
					target={usagesTarget.range}
					token={usagesTarget.token}
					urlRev={rev || null}
					onclose={() => (usagesTarget = null)}
				/>
			</div>
		{/if}
	{/if}
</div>

<style>
	.snippet {
		border: 1px solid var(--color-border);
		border-radius: var(--radius, 4px);
		overflow: hidden;
	}
	.snippet-header {
		display: flex;
		gap: var(--space-3);
		align-items: center;
		padding: var(--space-1) var(--space-3);
		background: var(--color-bg-subtle);
		border-bottom: 1px solid var(--color-border);
		font-size: 0.85rem;
	}
	.path {
		overflow-wrap: anywhere;
	}
	.code {
		max-height: 32rem;
		overflow: auto;
	}
	.code :global(.cm-editor) {
		max-height: 32rem;
	}
	.usages {
		height: 18rem;
		border-top: 1px solid var(--color-border);
	}
	.pad {
		padding: var(--space-2) var(--space-3);
		margin: 0;
	}
	.alert {
		margin: var(--space-2);
	}
</style>
