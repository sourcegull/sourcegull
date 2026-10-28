<script lang="ts">
	import { EVENTS, logEvent } from '#lib/events.ts';
	import CodeHostLink from '#lib/repo/CodeHostLink.svelte';
	import { externalURLs, type CodeHostTarget } from '#lib/repo/codehost.ts';
	import { repoPageURL, repoURL, shortRev } from '#lib/repo/url.ts';
	import { commitLines, mergeChunks, segments, type LineView } from './highlight.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { matchCount, type SearchMatch } from './stream.ts';

	interface Props {
		match: SearchMatch;
		/** Expand every result (for `count:` searches where the user asked for all). */
		expanded?: boolean;
	}

	let { match, expanded = false }: Props = $props();

	/** Chunk groups and commit lines shown before "Show more". */
	const COLLAPSED_GROUPS = 3;
	const COLLAPSED_LINES = 12;

	let showAll = $state(false);
	const all = $derived(expanded || showAll);

	function fileRev(m: { branches?: string[]; commit: string }): string | null {
		const b = m.branches?.[0];
		return b && b !== 'HEAD' ? b : null;
	}

	function blobURL(m: { repository: string; path: string; branches?: string[]; commit: string }, line?: number): string {
		const url = repoPageURL(m.repository, fileRev(m), 'blob', m.path);
		return line ? `${url}#L${line}` : url;
	}

	function dirAndName(path: string): [string, string] {
		const i = path.lastIndexOf('/');
		return i < 0 ? ['', path] : [path.slice(0, i + 1), path.slice(i + 1)];
	}

	const groups: LineView[][] = $derived(match.type === 'content' ? mergeChunks(match.chunkMatches ?? []) : []);
	const commit = $derived(match.type === 'commit' ? commitLines(match) : null);
	const subject = $derived(match.type === 'commit' ? (match.message.split('\n')[0] ?? '') : '');
	/** The code host link for the header icon: at the matched commit, first matched line. */
	const codeHostLink = $derived('repository' in match ? externalURLs.get(match.repository) : undefined);
	const codeHostTarget: CodeHostTarget = $derived.by(() => {
		switch (match.type) {
			case 'commit':
				return { kind: 'commit', oid: match.oid };
			case 'content':
			case 'path':
			case 'symbol': {
				const line =
					match.type === 'content'
						? groups.flat().find((l) => l.segments.some((s) => s.match))?.number
						: match.type === 'symbol'
							? match.symbols[0]?.line
							: undefined;
				return { kind: 'blob', rev: match.commit || fileRev(match) || 'HEAD', path: match.path, line };
			}
			default:
				return { kind: 'repo' };
		}
	});
	/** Matches in a file result, shown in the header ("3 matches"). */
	const count = $derived(match.type === 'content' || match.type === 'symbol' ? matchCount(match) : 0);
	const hiddenGroups = $derived(all ? 0 : Math.max(0, groups.length - COLLAPSED_GROUPS));
	const hiddenLines = $derived(all || !commit ? 0 : Math.max(0, commit.lines.length - COLLAPSED_LINES));

	/** A click on any link of the result is a usage event for the admin analytics. */
	function onclick(e: MouseEvent): void {
		if (e.target instanceof Element && e.target.closest('a')) {
			logEvent('SearchResultClicked', { type: match.type });
		}
	}

	function formatDate(s: string): string {
		const d = new Date(s);
		return Number.isNaN(d.getTime()) ? s : d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
	}
</script>

{#snippet lineText(line: LineView)}
	{#each line.segments as s, i (i)}{#if s.match}<mark>{s.text}</mark>{:else}{s.text}{/if}{/each}
{/snippet}

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
<article class="result" {onclick}>
	<header tabindex="-1" class="result-header">
		{#if match.type === 'repo'}
			<span class="type-icon" aria-hidden="true"><Icon name="repo" size={15} /></span>
			<span class="kind" title="Repository">repo</span>
			<a class="repo" href={repoURL(match.repository)}>{match.repository}</a>
			{#if match.private}<span class="badge">private</span>{/if}
			{#if match.fork}<span class="badge">fork</span>{/if}
			{#if match.archived}<span class="badge">archived</span>{/if}
			<CodeHostLink link={codeHostLink} target={codeHostTarget} />
		{:else if match.type === 'person'}
			<span class="kind" title="Owner">owner</span>
			{#if match.user}
				<a class="title" href="/users/{match.user.username}">{match.user.displayName || match.user.username}</a>
				<span class="muted mono">@{match.user.username}</span>
			{:else}
				<span class="title">{match.handle ? `@${match.handle}` : match.email}</span>
				<span class="badge">not a user</span>
			{/if}
			{#if match.user && match.handle && match.handle !== match.user.username}
				<span class="muted meta">as @{match.handle}</span>
			{/if}
		{:else if match.type === 'team'}
			<span class="kind" title="Owning team">team</span>
			<span class="title">{match.displayName || match.name || match.handle}</span>
			<span class="muted mono">@{match.name || match.handle}</span>
			{#if !match.name}<span class="badge">external</span>{/if}
		{:else if match.type === 'commit'}
			<span class="type-icon" aria-hidden="true"><Icon name="commit" size={15} /></span>
			<a class="repo" href={repoURL(match.repository)}>{match.repository}</a>
			<span class="sep">›</span>
			<a class="title" href={match.url}>{subject}</a>
			<span class="spacer"></span>
			<span class="muted meta">
				{match.authorName} · {formatDate(match.authorDate)} ·
				<a class="mono" href={match.url}>{shortRev(match.oid)}</a>
			</span>
			<CodeHostLink link={codeHostLink} target={codeHostTarget} />
		{:else}
			{@const [dir, name] = dirAndName(match.path)}
			<span class="type-icon" aria-hidden="true"><Icon name={match.type === 'symbol' ? 'symbol' : 'file'} size={15} /></span>
			<a class="repo" href={repoURL(match.repository, fileRev(match))}>{match.repository}</a>
			<span class="sep">›</span>
			<a class="path mono" href={blobURL(match)}>
				{#if match.type === 'path' && match.pathMatches?.length}
					{@const ranges = match.pathMatches.map((r) => [r.start.column, r.end.column] as [number, number])}
					{#each segments(match.path, ranges) as s, i (i)}{#if s.match}<mark>{s.text}</mark>{:else}{s.text}{/if}{/each}
				{:else}
					<span class="dir">{dir}</span><span class="name">{name}</span>
				{/if}
			</a>
			{#if fileRev(match)}<span class="badge mono">{fileRev(match)}</span>{/if}
			<span class="spacer"></span>
			{#if count > 0}
				<span class="match-count">{count} {match.type === 'symbol' ? 'symbol' : 'match'}{count === 1 ? '' : match.type === 'symbol' ? 's' : 'es'}</span>
			{/if}
			{#if match.language}<span class="muted meta">{match.language}</span>{/if}
			<CodeHostLink link={codeHostLink} target={codeHostTarget} />
		{/if}
	</header>

	{#if match.type === 'repo' && match.description}
		<p class="description muted">{match.description}</p>
	{:else if match.type === 'content' && groups.length > 0}
		<div class="code" oncopy={() => logEvent(EVENTS.codeCopied, { source: 'search' })}>
			{#each groups.slice(0, all ? undefined : COLLAPSED_GROUPS) as group, gi (gi)}
				<table class="chunk mono">
					<tbody>
						{#each group as line (line.number)}
							<tr>
								<td class="ln"><a href={blobURL(match, line.number)}>{line.number}</a></td>
								<td class="text">{@render lineText(line)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			{/each}
		</div>
	{:else if match.type === 'symbol'}
		<ul class="symbols">
			{#each match.symbols as s (s.url)}
				<li>
					<span class="badge symbol-kind" title={s.kind}>{s.kind.toLowerCase()}</span>
					<a class="mono" href={s.url}>{s.name}</a>
					{#if s.containerName}<span class="muted mono">{s.containerName}</span>{/if}
					<span class="muted">line {s.line}</span>
				</li>
			{/each}
		</ul>
	{:else if commit}
		<table class="chunk mono" class:diff={commit.language === 'diff'}>
			<tbody>
				{#each commit.lines.slice(0, all ? undefined : COLLAPSED_LINES) as line (line.number)}
					<tr class={line.marker === '+' ? 'added' : line.marker === '-' ? 'deleted' : line.marker === '@' ? 'hunk' : ''}>
						<td class="text">{@render lineText(line)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	{#if hiddenGroups > 0 || hiddenLines > 0}
		<button type="button" class="more" aria-expanded="false" onclick={() => (showAll = true)}>
			<Icon name="chevron-down" size={14} />
			Show {hiddenGroups > 0 ? `${hiddenGroups} more match${hiddenGroups === 1 ? '' : 'es'}` : `${hiddenLines} more lines`}
		</button>
	{:else if showAll}
		<button type="button" class="more" aria-expanded="true" onclick={() => (showAll = false)}>
			<span class="flip"><Icon name="chevron-down" size={14} /></span>
			Show less
		</button>
	{/if}
</article>

<style>
	.result {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-surface);
		box-shadow: var(--shadow-xs);
		overflow: hidden;
		transition: border-color var(--duration) var(--ease);
	}
	.result:hover {
		border-color: var(--color-border-strong);
	}
	.result-header {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-1) var(--space-2);
		padding: var(--space-2) var(--space-3);
		background: var(--color-bg-subtle);
		border-bottom: 1px solid var(--color-border);
		font-size: var(--text-sm);
		line-height: 1.4;
		outline: none;
		min-width: 0;
	}
	.result-header:last-child {
		border-bottom: 0;
	}
	.result-header:focus-visible {
		box-shadow: inset 0 0 0 2px var(--color-focus);
	}
	.type-icon {
		display: inline-flex;
		color: var(--color-text-subtle);
	}
	.repo {
		font-weight: var(--weight-medium);
		color: var(--color-text-muted);
		overflow-wrap: anywhere;
	}
	.repo:hover {
		color: var(--color-text);
	}
	.sep {
		color: var(--color-text-subtle);
	}
	.path {
		color: var(--color-text);
		overflow-wrap: anywhere;
		min-width: 0;
	}
	.path .dir {
		color: var(--color-text-muted);
	}
	.path .name,
	.title {
		font-weight: var(--weight-semibold);
		color: var(--color-text);
	}
	.path:hover .name,
	a.title:hover {
		color: var(--color-link);
	}
	.spacer {
		flex: 1;
	}
	.meta {
		font-size: var(--text-xs);
	}
	.match-count {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}
	.kind {
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--color-text-muted);
	}
	.description {
		margin: 0;
		padding: var(--space-2) var(--space-3);
		font-size: var(--text-sm);
	}
	.chunk {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--text-sm);
		line-height: 1.5;
	}
	.chunk + .chunk {
		border-top: 1px solid var(--color-border);
	}
	.chunk tr:first-child td {
		padding-top: var(--space-1);
	}
	.chunk tr:last-child td {
		padding-bottom: var(--space-1);
	}
	.ln {
		width: 1%;
		min-width: 3.5rem;
		padding: 0 var(--space-3) 0 var(--space-3);
		text-align: right;
		user-select: none;
		vertical-align: top;
		background: var(--editor-gutter);
		border-right: 1px solid var(--color-border);
	}
	.ln a {
		color: var(--color-text-subtle);
		font-variant-numeric: tabular-nums;
	}
	.ln a:hover {
		color: var(--color-text);
		text-decoration: none;
	}
	.text {
		padding: 0 var(--space-3);
		white-space: pre;
		overflow-wrap: normal;
	}
	tr:hover .text {
		background: var(--color-bg-hover);
	}
	.code,
	.chunk {
		overflow-x: auto;
		display: block;
	}
	.chunk tbody {
		display: table;
		width: 100%;
	}
	mark {
		background: var(--match-bg);
		box-shadow: 0 0 0 1px var(--match-ring);
		color: inherit;
		border-radius: 2px;
	}
	tr.added {
		background: var(--diff-added-bg);
	}
	tr.deleted {
		background: var(--diff-deleted-bg);
	}
	tr.hunk .text {
		color: var(--color-text-muted);
	}
	.symbols {
		list-style: none;
		margin: 0;
		padding: var(--space-2) var(--space-3);
		display: grid;
		gap: var(--space-1);
	}
	.symbols li {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		align-items: baseline;
		font-size: var(--text-sm);
	}
	.symbol-kind {
		font-size: 0.7rem;
	}
	.more {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		width: 100%;
		padding: var(--space-1) var(--space-3);
		border: 0;
		border-top: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
		font: inherit;
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		text-align: left;
		cursor: pointer;
		transition:
			color var(--duration) var(--ease),
			background-color var(--duration) var(--ease);
	}
	.more:hover {
		color: var(--color-text);
		background: var(--color-bg-hover);
	}
	.more:focus-visible {
		outline: none;
		box-shadow: inset 0 0 0 2px var(--color-focus);
	}
	.flip {
		display: inline-flex;
		transform: rotate(180deg);
	}
</style>
