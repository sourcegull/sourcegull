<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { REPOSITORIES, type RepositoriesResult, type RepositoryListNode } from '#lib/repo/queries.ts';

	const PAGE_SIZE = 50;

	let repos = $state.raw<RepositoryListNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let query = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let seq = 0;

	async function load(after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<
				RepositoriesResult,
				{ first: number; after: string | null; query: string | null }
			>(REPOSITORIES, { first: PAGE_SIZE, after, query: query.trim() || null });
			if (mine !== seq) return;
			repos = after ? [...repos, ...r.repositories.nodes] : r.repositories.nodes;
			totalCount = r.repositories.totalCount;
			cursor = r.repositories.pageInfo.endCursor;
			hasMore = r.repositories.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(null), 200);
	}

	onMount(() => {
		void load(null);
		return () => clearTimeout(debounce);
	});

	function status(r: RepositoryListNode): { label: string; cls: string } | null {
		if (r.mirrorInfo.cloneInProgress) return { label: 'Cloning', cls: 'badge-primary' };
		if (!r.mirrorInfo.cloned) return { label: 'Not cloned', cls: '' };
		if (r.mirrorInfo.lastError) return { label: 'Fetch failed', cls: 'badge-danger' };
		return null;
	}
</script>

<svelte:head><title>Repositories · Sourcegull</title></svelte:head>

<div class="repos-page">
	<div class="page-header">
		<h1>Repositories</h1>
		<span class="muted">{totalCount} total</span>
	</div>

	<div class="field">
		<label class="visually-hidden" for="repo-filter">Filter repositories</label>
		<input
			id="repo-filter"
			class="input"
			type="search"
			placeholder="Filter by name…"
			bind:value={query}
			{oninput}
		/>
	</div>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}

	<ul class="repo-list" aria-busy={loading}>
		{#each repos as r (r.id)}
			{@const s = status(r)}
			<li>
				<div class="title">
					<a href={r.url}>{r.name}</a>
					{#if r.isPrivate}<span class="badge">private</span>{/if}
					{#if r.isFork}<span class="badge">fork</span>{/if}
					{#if r.isArchived}<span class="badge">archived</span>{/if}
					{#if s}<span class="badge {s.cls}">{s.label}</span>{/if}
				</div>
				{#if r.description}<p class="muted description">{r.description}</p>{/if}
			</li>
		{:else}
			<li class="muted">
				{loading ? 'Loading…' : query.trim() ? 'No repositories match.' : 'No repositories yet. Site admins can add a GitLab connection.'}
			</li>
		{/each}
	</ul>

	{#if hasMore}
		<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
			{loading ? 'Loading…' : 'Show more'}
		</button>
	{/if}
</div>

<style>
	.repos-page {
		max-width: 60rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
	}
	.repo-list {
		list-style: none;
		padding: 0;
		margin: 0 0 var(--space-4);
	}
	.repo-list li {
		padding: var(--space-3) 0;
		border-bottom: 1px solid var(--color-border);
	}
	.title {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		font-weight: 600;
	}
	.description {
		margin: var(--space-1) 0 0;
	}
	:global(.badge-danger) {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
</style>
