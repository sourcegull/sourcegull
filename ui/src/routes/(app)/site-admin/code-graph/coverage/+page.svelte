<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		PRECISE_INDEX_COVERAGE,
		type PreciseIndexCoverageNode,
		type PreciseIndexCoverageResult,
		type PreciseIndexCoverageState
	} from '#lib/queries.ts';
	import { COVERAGE_STATES, ago, indexedSummary, stateInfo } from '#lib/codeintel/coverage.ts';

	const PAGE_SIZE = 50;

	type Coverage = PreciseIndexCoverageResult['preciseIndexCoverage'];
	type Variables = {
		staleAfterDays: number;
		state: PreciseIndexCoverageState | null;
		query: string | null;
		first: number;
		after: string | null;
	};

	let coverage = $state<Coverage | null>(null);
	let nodes = $state<PreciseIndexCoverageNode[]>([]);
	let stateFilter = $state<PreciseIndexCoverageState | ''>('');
	let query = $state('');
	let staleAfterDays = $state(7);
	let loading = $state(true);
	let error = $state<string | null>(null);
	const now = new Date();

	async function load(after: string | null = null): Promise<void> {
		loading = true;
		error = null;
		try {
			const result = await graphql<PreciseIndexCoverageResult, Variables>(PRECISE_INDEX_COVERAGE, {
				staleAfterDays: Math.max(1, Math.round(staleAfterDays) || 7),
				state: stateFilter || null,
				query: query.trim() || null,
				first: PAGE_SIZE,
				after
			});
			coverage = result.preciseIndexCoverage;
			const page = result.preciseIndexCoverage.repositories.nodes;
			nodes = after ? [...nodes, ...page] : page;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	function count(c: Coverage['counts'], s: PreciseIndexCoverageState): number {
		switch (s) {
			case 'UP_TO_DATE':
				return c.upToDate;
			case 'STALE':
				return c.stale;
			case 'FAILING':
				return c.failing;
			case 'NONE':
				return c.none;
			case 'NOT_TARGETED':
				return c.notTargeted;
		}
	}

	function pick(s: PreciseIndexCoverageState | ''): void {
		stateFilter = stateFilter === s ? '' : s;
		void load();
	}

	onMount(() => void load());
</script>

<svelte:head><title>Index coverage · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Precise index coverage</h1>
		<p class="page-description">
			Which repositories have a current precise index on their default branch, which fail to
			index, and which have none. Code navigation in repositories without a current index falls
			back to search-based results. Policies are on the
			<a href="/site-admin/code-graph">Code graph policies</a> page.
		</p>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if coverage && !coverage.autoIndexingEnabled}
	<div class="alert alert-info" role="status">
		Auto-indexing is off (<code>codeIntelAutoIndexing.enabled</code>), so no repository is
		targeted by indexing policies; only uploaded indexes are counted.
	</div>
{/if}

{#if coverage}
	<div class="stats" data-testid="coverage-counts">
		{#each COVERAGE_STATES as s (s.state)}
			<button
				type="button"
				class="card stat"
				class:selected={stateFilter === s.state}
				aria-pressed={stateFilter === s.state}
				title={s.help}
				onclick={() => pick(s.state)}
			>
				<span class="stat-value">{count(coverage.counts, s.state).toLocaleString('en-US')}</span>
				<span class="stat-label">{s.label}</span>
			</button>
		{/each}
	</div>
{/if}

<form
	class="toolbar filters"
	onsubmit={(e) => {
		e.preventDefault();
		void load();
	}}
>
	<input
		class="input"
		type="search"
		placeholder="Filter by repository name"
		aria-label="Filter repositories"
		bind:value={query}
	/>
	<select
		class="select"
		aria-label="Coverage state"
		bind:value={stateFilter}
		onchange={() => void load()}
	>
		<option value="">All states</option>
		{#each COVERAGE_STATES as s (s.state)}
			<option value={s.state}>{s.label}</option>
		{/each}
	</select>
	<label class="days">
		Stale after
		<input
			class="input"
			type="number"
			min="1"
			max="365"
			bind:value={staleAfterDays}
			onchange={() => void load()}
		/>
		days
	</label>
</form>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Repository</th>
				<th scope="col">State</th>
				<th scope="col">Last indexed commit</th>
				<th scope="col">Last failure</th>
			</tr>
		</thead>
		<tbody>
			{#each nodes as n (n.repositoryID)}
				{@const info = stateInfo(n.state)}
				<tr>
					<td><a href={n.codeGraphURL}>{n.repositoryName}</a></td>
					<td><span class="badge {info.badge}" title={info.help}>{info.label}</span></td>
					<td class="mono small" title={n.indexedAt ? new Date(n.indexedAt).toLocaleString() : ''}
						>{indexedSummary(n, now)}</td
					>
					<td class="small">
						{#if n.failureMessage !== null || n.failedAt}
							<div class="failure">{n.failureMessage || 'no message'}</div>
							<div class="muted">
								{n.failureSource === 'index_job' ? 'index job' : 'upload'}
								{#if n.failedAt}· {ago(n.failedAt, now)}{/if}
							</div>
						{:else}
							<span class="muted">none</span>
						{/if}
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="4" class="empty-cell"
						>{loading ? 'Loading…' : 'No repositories match.'}</td
					>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

{#if coverage}
	<div class="footer">
		<span class="muted">{nodes.length} of {coverage.repositories.totalCount} repositories</span>
		{#if coverage.repositories.pageInfo.hasNextPage}
			<button
				class="btn btn-sm"
				disabled={loading}
				onclick={() => void load(coverage?.repositories.pageInfo.endCursor ?? null)}
				>Show more</button
			>
		{/if}
	</div>
{/if}

<style>
	.stats {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
		gap: var(--space-3);
		margin-bottom: var(--space-5);
	}
	.stat {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: var(--space-1);
		padding: var(--space-3) var(--space-4);
		text-align: left;
		cursor: pointer;
		font: inherit;
		color: inherit;
	}
	.stat.selected {
		border-color: var(--color-primary);
	}
	.stat-value {
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
	}
	.stat-label {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}
	.filters {
		gap: var(--space-3);
		margin-bottom: var(--space-4);
		flex-wrap: wrap;
	}
	.filters input[type='search'] {
		flex: 1 1 14rem;
		max-width: 20rem;
	}
	.days {
		display: inline-flex;
		gap: var(--space-2);
		align-items: center;
	}
	.days input {
		width: 5rem;
	}
	.small {
		font-size: var(--text-sm);
	}
	.failure {
		color: var(--color-danger);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		max-width: 40rem;
	}
	.footer {
		display: flex;
		gap: var(--space-3);
		align-items: center;
		margin-top: var(--space-3);
	}
</style>
