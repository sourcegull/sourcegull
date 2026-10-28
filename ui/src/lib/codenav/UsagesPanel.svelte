<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { repoPageURL } from '#lib/repo/url.ts';
	import {
		provenanceLabel,
		USAGES_FOR_SYMBOL,
		type RangeInput,
		type UsageCoverage,
		type UsageKind,
		type UsageNode,
		type UsagesResult
	} from './queries.ts';

	interface Props {
		/** The usage to start from. */
		target: RangeInput;
		/** The identifier, for the heading. */
		token: string;
		/** The revision to use in links to the current repository and commit (e.g. a branch). */
		urlRev: string | null;
		onclose: () => void;
	}

	let { target, token, urlRev, onclose }: Props = $props();

	const PAGE = 100;

	let usages = $state.raw<UsageNode[]>([]);
	let endCursor = $state<string | null>(null);
	let coverage = $state.raw<UsageCoverage | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let seq = 0;

	$effect(() => {
		const t = target;
		usages = [];
		endCursor = null;
		coverage = null;
		void load(t, null, ++seq);
	});

	async function load(t: RangeInput, after: string | null, mine: number): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<UsagesResult, { range: RangeInput; first: number; after: string | null }>(
				USAGES_FOR_SYMBOL,
				{ range: t, first: PAGE, after }
			);
			if (mine !== seq) return;
			usages = [...usages, ...r.usagesForSymbol.nodes];
			coverage = r.usagesForSymbol.coverage;
			endCursor = r.usagesForSymbol.pageInfo.hasNextPage ? r.usagesForSymbol.pageInfo.endCursor : null;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	interface FileGroup {
		repo: string;
		revision: string;
		path: string;
		/** Search-based usages in a precise answer (from a repository without precise data). */
		searchBased: boolean;
		usages: UsageNode[];
	}

	/** Usages of one kind, grouped by file in the server's order. */
	function groups(kind: UsageKind): FileGroup[] {
		const out: FileGroup[] = [];
		for (const u of usages) {
			if (u.usageKind !== kind) continue;
			const r = u.usageRange;
			const last = out[out.length - 1];
			const searchBased = usages[0]?.provenance === 'PRECISE' && u.provenance !== 'PRECISE';
			if (
				last &&
				last.repo === r.repository &&
				last.path === r.path &&
				last.revision === r.revision &&
				last.searchBased === searchBased
			) {
				last.usages.push(u);
			} else {
				out.push({ repo: r.repository, revision: r.revision, path: r.path, searchBased, usages: [u] });
			}
		}
		return out;
	}

	const count = (list: FileGroup[]): number => list.reduce((n, g) => n + g.usages.length, 0);
	/**
	 * Definitions and references always; implementations (of an interface or base class) and the
	 * symbols this one implements when the index has them.
	 */
	const sections = $derived(
		[
			{ title: 'Definitions', list: groups('DEFINITION'), always: true },
			{ title: 'References', list: groups('REFERENCE'), always: true },
			{ title: 'Implementations', list: groups('IMPLEMENTATION'), always: false },
			{ title: 'Implements', list: groups('SUPER'), always: false }
		]
			.filter((sec) => sec.always || sec.list.length > 0)
			.map((sec) => ({ ...sec, count: count(sec.list) }))
	);

	function usageURL(u: UsageNode): string {
		const r = u.usageRange;
		const sameCommit = r.repository === target.repository && r.revision === target.revision;
		const rev = sameCommit ? urlRev : r.revision;
		const s = r.range.start;
		return `${repoPageURL(r.repository, rev, 'blob', r.path)}#L${s.line + 1}:${s.character + 1}`;
	}

	/** The usage's line split around the usage. */
	function parts(u: UsageNode): [string, string, string] {
		const text = u.surroundingContent;
		const r = u.usageRange.range;
		const end = r.end.line === r.start.line ? r.end.character : text.length;
		const lead = text.length - text.trimStart().length;
		const from = Math.max(r.start.character, 0);
		const start = Math.min(lead, from);
		return [text.slice(start, from), text.slice(from, end), text.slice(end).trimEnd()];
	}

	function more(): void {
		if (endCursor) void load(target, endCursor, seq);
	}
</script>

<section class="usages" aria-label="References to {token}">
	<header>
		<h2>
			References to <code>{token}</code>
		</h2>
		{#if usages.length > 0}
			<span class="badge">{provenanceLabel(usages[0]?.provenance ?? 'SEARCH_BASED')}</span>
		{/if}
		<div class="spacer"></div>
		<button type="button" class="btn btn-sm" onclick={onclose} aria-label="Close references">Close</button>
	</header>
	<div class="body">
		{#if coverage && (coverage.searchBasedRepositories.length > 0 || coverage.crossVersionRepositories.length > 0 || coverage.truncated)}
			<p class="muted coverage">Coverage: {coverage.summary}</p>
		{/if}
		{#if error}
			<div class="alert alert-danger" role="alert">{error}</div>
		{/if}
		{#each sections as section (section.title)}
			<h3>{section.title} <span class="muted">({section.count}{endCursor ? '+' : ''})</span></h3>
			{#if section.list.length === 0 && !loading}
				<p class="muted none">None found.</p>
			{/if}
			{#each section.list as g (`${g.repo}@${g.revision}/${g.path}`)}
				<div class="file">
					<div class="file-name">
						{#if g.repo !== target.repository}<span class="repo">{g.repo}</span> ›{/if}
						<a href={repoPageURL(g.repo, g.repo === target.repository && g.revision === target.revision ? urlRev : g.revision, 'blob', g.path)}>{g.path}</a>
						{#if g.searchBased}<span class="badge">{provenanceLabel('SEARCH_BASED')}</span>{/if}
					</div>
					<ul>
						{#each g.usages as u (`${u.usageRange.range.start.line}:${u.usageRange.range.start.character}`)}
							{@const [before, match, after] = parts(u)}
							<li>
								<a class="line" href={usageURL(u)}>
									<span class="num">{u.usageRange.range.start.line + 1}</span>
									<code>{before}<mark>{match}</mark>{after}</code>
								</a>
							</li>
						{/each}
					</ul>
				</div>
			{/each}
		{/each}
		{#if loading}
			<p class="muted none">Loading…</p>
		{:else if endCursor}
			<button type="button" class="btn btn-sm more" onclick={more}>Load more</button>
		{/if}
	</div>
</section>

<style>
	.usages {
		display: flex;
		flex-direction: column;
		min-height: 0;
		height: 100%;
		border-top: 1px solid var(--color-border);
		background: var(--color-bg);
	}
	header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) var(--space-4);
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
	}
	h2 {
		font-size: 0.95em;
		margin: 0;
		font-weight: 600;
	}
	h3 {
		font-size: 0.85em;
		margin: var(--space-3) var(--space-4) var(--space-1);
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.spacer {
		flex: 1;
	}
	.badge {
		font-size: 0.75em;
		color: var(--color-text-muted);
		border: 1px solid var(--color-border);
		border-radius: 999px;
		padding: 0 var(--space-2);
	}
	.body {
		overflow: auto;
		flex: 1;
		min-height: 0;
		padding-bottom: var(--space-4);
	}
	.none {
		margin: var(--space-1) var(--space-4);
	}
	.file {
		margin: 0 var(--space-4) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		overflow: hidden;
	}
	.file-name {
		padding: var(--space-1) var(--space-2);
		background: var(--color-bg-subtle);
		border-bottom: 1px solid var(--color-border);
		font-size: 0.9em;
		overflow-wrap: anywhere;
	}
	.repo {
		color: var(--color-text-muted);
	}
	.coverage {
		margin: 0 0 0.5rem;
		font-size: 0.85em;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.line {
		display: flex;
		gap: var(--space-3);
		padding: 2px var(--space-2);
		color: var(--color-text);
		text-decoration: none;
		font-size: 12.5px;
	}
	.line:hover {
		background: var(--color-bg-muted);
	}
	.num {
		flex: 0 0 3.5em;
		text-align: right;
		color: var(--color-text-muted);
		font-family: var(--font-mono);
	}
	.line code {
		font-family: var(--font-mono);
		white-space: pre;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	mark {
		background: var(--editor-line-selected);
		color: inherit;
		border-radius: 2px;
	}
	.more {
		margin: var(--space-2) var(--space-4);
	}
	.alert {
		margin: var(--space-2) var(--space-4);
	}
</style>
