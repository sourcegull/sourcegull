<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import CommitTable from './CommitTable.svelte';
	import { cloneStateMessage, HISTORY, type CommitSummary, type HistoryResult } from './queries.ts';
	import { repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		urlRev: string | null;
		path: string;
	}

	let { repo, urlRev, path }: Props = $props();

	const PAGE = 50;

	let commits = $state.raw<CommitSummary[]>([]);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let seq = 0;

	$effect(() => {
		commits = [];
		cursor = null;
		void load(repo, urlRev ?? 'HEAD', path, null);
	});

	async function load(repoName: string, rev: string, p: string, after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<
				HistoryResult,
				{ repo: string; rev: string; path: string | null; first: number; after: string | null }
			>(HISTORY, { repo: repoName, rev, path: p || null, first: PAGE, after });
			if (mine !== seq) return;
			const a = r.repository?.commit?.ancestors;
			if (!a) {
				error = `Revision ${rev} not found.`;
				return;
			}
			commits = after ? [...commits, ...a.nodes] : a.nodes;
			cursor = a.pageInfo.endCursor;
			hasMore = a.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}
</script>

<div class="commit-list">
	<div class="page-header">
		<h2>
			History
			{#if path}<span class="muted">of</span>
				<a class="mono" href={repoPageURL(repo, urlRev, 'blob', path)}>{path}</a>{/if}
		</h2>
	</div>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	<CommitTable {repo} {commits} emptyText={loading ? 'Loading…' : 'No commits.'} />
	{#if hasMore}
		<p>
			<button
				type="button"
				class="btn"
				disabled={loading}
				onclick={() => load(repo, urlRev ?? 'HEAD', path, cursor)}>{loading ? 'Loading…' : 'Show more'}</button
			>
		</p>
	{/if}
</div>

<style>
	.commit-list {
		padding: var(--space-4);
		max-width: 72rem;
	}
</style>
