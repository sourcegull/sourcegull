<script lang="ts">
	import { page } from '$app/state';
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { searchQueryURL } from '#lib/search/query.ts';
	import {
		CANCEL_SEARCH_JOB,
		DELETE_SEARCH_JOB,
		SEARCH_JOBS,
		csvURL,
		isActive,
		progressPercent,
		type SearchJobNode,
		type SearchJobState,
		type SearchJobsResult,
		type SearchJobsVariables
	} from '#lib/search/search-jobs.ts';

	const PAGE_SIZE = 50;
	/** How often the list refreshes while a job runs. */
	const POLL_MS = 5000;

	const user = $derived(page.data.auth?.user ?? null);

	let items = $state.raw<SearchJobNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let filter = $state('');
	let stateFilter = $state<SearchJobState | ''>('');
	let orderBy = $state<SearchJobsVariables['orderBy']>('CREATED_AT');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let seq = 0;

	async function load(after: string | null, quiet = false): Promise<void> {
		const mine = ++seq;
		if (!quiet) loading = true;
		error = null;
		try {
			const r = await graphql<SearchJobsResult, SearchJobsVariables>(SEARCH_JOBS, {
				first: after ? PAGE_SIZE : Math.max(PAGE_SIZE, items.length),
				after,
				query: filter.trim() || null,
				states: stateFilter ? [stateFilter] : null,
				orderBy,
				descending: true
			});
			if (mine !== seq) return;
			items = after ? [...items, ...r.searchJobs.nodes] : r.searchJobs.nodes;
			totalCount = r.searchJobs.totalCount;
			cursor = r.searchJobs.pageInfo.endCursor;
			hasMore = r.searchJobs.pageInfo.hasNextPage;
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
		const timer = setInterval(() => {
			if (items.some((j) => isActive(j.state)) && !busy) void load(null, true);
		}, POLL_MS);
		return () => {
			clearTimeout(debounce);
			clearInterval(timer);
		};
	});

	async function cancel(job: SearchJobNode): Promise<void> {
		busy = job.id;
		try {
			await graphql(CANCEL_SEARCH_JOB, { id: job.id });
			await load(null, true);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function remove(job: SearchJobNode): Promise<void> {
		if (!confirm(`Delete the search job for “${job.query}” and its results?`)) return;
		busy = job.id;
		try {
			await graphql(DELETE_SEARCH_JOB, { id: job.id });
			await load(null, true);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	function formatDate(s: string): string {
		return new Date(s).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
	}

	function stateBadge(state: SearchJobState): string {
		switch (state) {
			case 'COMPLETED':
				return 'badge-success';
			case 'FAILED':
				return 'badge-danger';
			case 'PROCESSING':
				return 'badge-info';
			case 'QUEUED':
				return 'badge-warning';
			default:
				return '';
		}
	}

	const STATES: SearchJobState[] = ['QUEUED', 'PROCESSING', 'COMPLETED', 'FAILED', 'CANCELED'];
</script>

<svelte:head><title>Search jobs · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Search jobs</h1>
			<p class="page-description">
				Run a query over every repository and revision it covers, without the limits and timeouts of interactive search,
				and download the results as JSON lines or CSV. Start one with “Create search job” on a results page.
			</p>
		</div>
		<div class="page-actions">
			<a class="btn btn-primary" href="/search"><Icon name="search" size={16} />New search</a>
		</div>
	</div>

	{#if !user}
		<div class="alert alert-info" role="status">Sign in to see your search jobs.</div>
	{:else}
		<div class="filter-bar">
			<label class="visually-hidden" for="sj-filter">Filter search jobs</label>
			<input id="sj-filter" class="input" type="search" placeholder="Filter by query…" bind:value={filter} {oninput} />
			<label class="visually-hidden" for="sj-state">State</label>
			<select id="sj-state" class="input" bind:value={stateFilter} onchange={() => load(null)}>
				<option value="">All states</option>
				{#each STATES as s (s)}<option value={s}>{s.toLowerCase()}</option>{/each}
			</select>
			<label class="visually-hidden" for="sj-order">Sort by</label>
			<select id="sj-order" class="input" bind:value={orderBy} onchange={() => load(null)}>
				<option value="CREATED_AT">Newest</option>
				<option value="QUERY">Query</option>
				<option value="STATE">State</option>
			</select>
			<span class="muted small count">{totalCount} total</span>
		</div>

		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

		{#if items.length === 0 && !loading && !error && !filter.trim() && !stateFilter}
			<div class="empty-state">
				<span class="icon"><Icon name="jobs" size={32} /></span>
				<h3>No search jobs yet</h3>
				<p>Run a search, then choose “Create search job” to get every result across all repositories.</p>
				<a class="btn btn-primary" href="/search"><Icon name="search" size={16} />Go to search</a>
			</div>
		{:else}
			<ul class="list list-group" aria-busy={loading}>
				{#each items as job (job.id)}
					{@const pct = progressPercent(job.repoStats)}
					<li>
						<div class="list-item-title">
							<span class="badge state state-{job.state.toLowerCase()} {stateBadge(job.state)}"
								>{job.state.toLowerCase()}</span
							>
							<a class="query" href={searchQueryURL(job.query)}><code class="mono">{job.query}</code></a>
						</div>
						<div class="progress-row">
							<progress max="100" value={pct} aria-label="Progress of {job.query}"></progress>
							<span class="muted small">
								{job.repoStats.completed + job.repoStats.failed} / {job.repoStats.total} revisions
								{#if job.repoStats.failed > 0}· {job.repoStats.failed} failed{/if}
							</span>
						</div>
						<p class="list-item-meta">
							{job.creator ? `${job.creator.username} · ` : ''}created {formatDate(job.createdAt)}
							{#if job.finishedAt}· finished {formatDate(job.finishedAt)}{/if}
						</p>
						<div class="actions">
							{#if job.URL && job.state !== 'QUEUED'}
								<a class="btn btn-sm" href={job.URL} download>Download JSONL</a>
								<a class="btn btn-sm" href={csvURL(job.URL)} download>Download CSV</a>
							{/if}
							{#if job.logURL && job.state !== 'QUEUED'}
								<a class="btn btn-sm btn-ghost" href={job.logURL} download>Log</a>
							{/if}
							<span class="spacer"></span>
							{#if isActive(job.state)}
								<button type="button" class="btn btn-sm" disabled={busy === job.id} onclick={() => cancel(job)}
									>Cancel</button
								>
							{/if}
							<button
								type="button"
								class="btn btn-sm btn-danger"
								disabled={busy === job.id}
								onclick={() => remove(job)}>Delete</button
							>
						</div>
					</li>
				{:else}
					<li class="empty">
						{loading ? 'Loading…' : 'No search jobs match.'}
					</li>
				{/each}
			</ul>
		{/if}

		{#if hasMore}
			<div class="more">
				<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
					{loading ? 'Loading…' : 'Show more'}
				</button>
			</div>
		{/if}
	{/if}
</div>

<style>
	.spacer {
		flex: 1;
	}
	.count {
		margin-left: auto;
	}
	.query {
		overflow-wrap: anywhere;
		min-width: 0;
		font-weight: var(--weight-medium);
	}
	.progress-row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
	}
	progress {
		appearance: none;
		width: 12rem;
		max-width: 100%;
		height: 6px;
		border: 0;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		overflow: hidden;
	}
	progress::-webkit-progress-bar {
		background: var(--color-bg-muted);
	}
	progress::-webkit-progress-value {
		background: var(--color-primary);
	}
	progress::-moz-progress-bar {
		background: var(--color-primary);
	}
	.actions {
		display: flex;
		gap: var(--space-2);
		flex-wrap: wrap;
		align-items: center;
		margin-top: var(--space-1);
	}
	.more {
		margin-top: var(--space-4);
		text-align: center;
	}
</style>
