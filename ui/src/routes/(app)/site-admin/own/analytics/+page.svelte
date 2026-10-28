<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		OWNERSHIP_ANALYTICS,
		coverage,
		type OwnershipAnalyticsResult,
		type OwnershipStats
	} from '#lib/own/queries.ts';

	// Ownership analytics (Code Ownership): how many files of the repositories' default
	// branches have an owner, for the instance and by repository. Computed in the background
	// when the analytics signal is on.

	type Vars = Record<string, unknown>;
	type Row = OwnershipAnalyticsResult['ownershipStatsByRepository']['nodes'][number];

	let instance = $state<OwnershipStats | null>(null);
	let rows = $state<Row[]>([]);
	let totalCount = $state(0);
	let endCursor = $state<string | null>(null);
	let hasNextPage = $state(false);
	let enabled = $state(true);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let sort = $state<'name' | 'coverage'>('name');

	const PAGE = 100;

	async function load(after: string | null): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<OwnershipAnalyticsResult, Vars>(OWNERSHIP_ANALYTICS, { first: PAGE, after });
			instance = r.instanceOwnershipStats;
			const conn = r.ownershipStatsByRepository;
			rows = after ? [...rows, ...conn.nodes] : conn.nodes;
			totalCount = conn.totalCount;
			endCursor = conn.pageInfo.endCursor;
			hasNextPage = conn.pageInfo.hasNextPage;
			enabled = r.ownSignalConfigurations.some((c) => c.name === 'analytics' && c.isEnabled);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	const sorted = $derived(
		sort === 'name'
			? rows
			: [...rows].sort(
					(a, b) =>
						(coverage(a.stats.totalOwnedFiles, a.stats.totalFiles) ?? 101) -
						(coverage(b.stats.totalOwnedFiles, b.stats.totalFiles) ?? 101)
				)
	);

	function pct(owned: number, total: number): string {
		const c = coverage(owned, total);
		return c === null ? '–' : `${c}%`;
	}

	const formatDate = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : '–');

	onMount(() => {
		void load(null);
	});
</script>

<svelte:head><title>Ownership analytics · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Ownership analytics</h1>
		<p class="page-description">
			Files on each repository's default branch with an owner from a CODEOWNERS file or an assignment.
			Updated daily and after pushes.
		</p>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if !loading && !enabled}
	<div class="alert alert-warning" role="status">
		The analytics signal is off: turn it on in <a href="/site-admin/own/signals">Ownership signals</a>.
	</div>
{/if}

{#if instance}
	<div class="tiles">
		<div class="card tile">
			<span class="value">{pct(instance.totalOwnedFiles, instance.totalFiles)}</span>
			<span class="tile-label">files owned ({instance.totalOwnedFiles} of {instance.totalFiles})</span>
		</div>
		<div class="card tile">
			<span class="value">{instance.totalCodeownedFiles}</span>
			<span class="tile-label">owned through CODEOWNERS</span>
		</div>
		<div class="card tile">
			<span class="value">{instance.totalAssignedOwnershipFiles}</span>
			<span class="tile-label">with assigned owners</span>
		</div>
		<div class="card tile">
			<span class="value small">{formatDate(instance.updatedAt)}</span>
			<span class="tile-label">last updated</span>
		</div>
	</div>
{/if}

<div class="section-header">
	<h2>By repository <span class="badge">{totalCount}</span></h2>
	<label class="sort">
		<span class="muted">Sort</span>
		<select class="input input-sm" bind:value={sort}>
			<option value="name">Name</option>
			<option value="coverage">Lowest coverage first (loaded)</option>
		</select>
	</label>
</div>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Repository</th>
				<th scope="col" class="num">Coverage</th>
				<th scope="col" class="num">Files</th>
				<th scope="col" class="num">CODEOWNERS</th>
				<th scope="col" class="num">Assigned</th>
				<th scope="col">Updated</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as r (r.name)}
				{@const c = coverage(r.stats.totalOwnedFiles, r.stats.totalFiles)}
				<tr>
					<td class="mono">
						{#if r.repository}<a href={r.repository.url}>{r.name}</a>{:else}{r.name}{/if}
					</td>
					<td class="num">
						<span class="bar" aria-hidden="true"><span style:width="{c ?? 0}%"></span></span>
						{pct(r.stats.totalOwnedFiles, r.stats.totalFiles)}
					</td>
					<td class="num">{r.stats.totalFiles}</td>
					<td class="num">{r.stats.totalCodeownedFiles}</td>
					<td class="num">{r.stats.totalAssignedOwnershipFiles}</td>
					<td>{formatDate(r.stats.updatedAt)}</td>
				</tr>
			{:else}
				<tr>
					<td colspan="6" class="empty-cell">{loading ? 'Loading…' : 'No repository has been analyzed yet.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>
{#if hasNextPage}
	<div class="more"><button class="btn" type="button" disabled={loading} onclick={() => load(endCursor)}>Load more</button></div>
{/if}

<style>
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
		gap: var(--space-3);
		margin: 0 0 var(--space-4);
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-4);
	}
	.value {
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.value.small {
		font-size: var(--text-md);
	}
	.tile-label {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}
	.section-header h2 {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.sort {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.bar {
		display: inline-block;
		width: 4rem;
		height: 0.5rem;
		margin-right: var(--space-2);
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		overflow: hidden;
		vertical-align: middle;
	}
	.bar span {
		display: block;
		height: 100%;
		background: var(--color-primary);
	}
	.more {
		margin-top: var(--space-3);
	}
</style>
