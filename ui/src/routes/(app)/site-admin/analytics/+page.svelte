<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import StatChart from '#lib/analytics/StatChart.svelte';
	import {
		ANALYTICS_OVERVIEW,
		DATE_RANGES,
		formatCount,
		formatLatency,
		preciseShare,
		type AnalyticsOverview,
		type DateRange,
		type Grouping
	} from '#lib/analytics/queries.ts';

	// Usage analytics: computed from this instance's own event logs, rolled up by the
	// worker every half hour. Nothing leaves the instance.

	type Data = AnalyticsOverview['site'];

	let range = $state<DateRange>('LAST_MONTH');
	let grouping = $state<Grouping>('DAILY');
	let data = $state<Data | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(r: DateRange, g: Grouping): Promise<void> {
		loading = true;
		error = null;
		try {
			const res = await graphql<AnalyticsOverview, Record<string, unknown>>(ANALYTICS_OVERVIEW, {
				range: r,
				grouping: g
			});
			data = res.site;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void load(range, grouping);
	});

	const a = $derived(data?.analytics ?? null);
	const usage = $derived(data?.usageStatistics ?? null);
	const share = $derived(
		a ? preciseShare(a.codeIntel.preciseEvents.summary.totalCount, a.codeIntel.searchBasedEvents.summary.totalCount) : null
	);
	const formatDate = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : 'not yet');
</script>

<svelte:head><title>Analytics · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Analytics</h1>
		<p class="page-description">
			Usage of this instance from its own event logs; nothing is sent anywhere. Updated every half
			hour (last: {formatDate(a?.updatedAt ?? null)}). Dates are UTC.
		</p>
		<RelatedLinks />
	</div>
	<div class="page-actions">
		<a class="btn btn-secondary" href="/site-admin/analytics/users">User activity</a>
	</div>
</div>

<div class="filters" role="group" aria-label="Date range and grouping">
	<div class="btn-group">
		{#each DATE_RANGES as r (r.value)}
			<button type="button" class="btn btn-sm" class:btn-primary={range === r.value} aria-pressed={range === r.value} onclick={() => (range = r.value)}>
				{r.label}
			</button>
		{/each}
	</div>
	<label>
		<span class="muted">Group by</span>
		<select class="input input-sm" bind:value={grouping}>
			<option value="DAILY">Day</option>
			<option value="WEEKLY">Week</option>
		</select>
	</label>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if a && usage}
	<section aria-busy={loading}>
		<div class="section-header"><h2>Users</h2></div>
		<div class="tiles">
			<div class="tile"><span class="value">{formatCount(usage.daus[0]?.userCount ?? 0)}</span><span class="label">active today</span></div>
			<div class="tile"><span class="value">{formatCount(usage.waus[0]?.userCount ?? 0)}</span><span class="label">active this week</span></div>
			<div class="tile"><span class="value">{formatCount(usage.maus[0]?.userCount ?? 0)}</span><span class="label">active this month</span></div>
			<div class="tile">
				<span class="value">{formatCount(a.users.activity.summary.totalRegisteredUsers)}</span>
				<span class="label">registered users active in range</span>
			</div>
		</div>
		<div class="grid">
			<StatChart title="Active users" item={a.users.activity} {grouping} metric="uniqueUsers" />
			<StatChart title="Sign-ins" item={a.users.signIns} {grouping} />
		</div>
		<div class="grid">
			<div>
				<h3>Days used in range</h3>
				<table class="table table-bordered table-sm">
					<thead><tr><th scope="col" class="num">Days</th><th scope="col" class="num">Users</th><th scope="col" class="num">Share</th></tr></thead>
					<tbody>
						{#each a.users.frequencies as f (f.daysUsed)}
							<tr><td class="num">{f.daysUsed}</td><td class="num">{f.frequency}</td><td class="num">{f.percentage.toFixed(1)}%</td></tr>
						{:else}
							<tr><td colspan="3" class="empty-cell">No activity in this range.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div>
				<h3>Monthly active users</h3>
				<table class="table table-bordered table-sm">
					<thead><tr><th scope="col">Month</th><th scope="col" class="num">Users</th></tr></thead>
					<tbody>
						{#each a.users.monthlyActiveUsers as m (m.date)}
							<tr><td>{m.date}</td><td class="num">{m.count}</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
		</div>
	</section>

	<section>
		<div class="section-header"><h2>Search</h2></div>
		<div class="grid">
			<StatChart title="Searches" item={a.search.searches} {grouping} />
			<StatChart title="Result clicks" item={a.search.resultClicks} {grouping} />
			<StatChart title="File views" item={a.search.fileViews} {grouping} />
		</div>
		<div class="grid">
			<div>
				<h3>Searches by type</h3>
				<table class="table table-bordered table-sm">
					<thead><tr><th scope="col">Type</th><th scope="col" class="num">Searches</th></tr></thead>
					<tbody>
						{#each a.search.searchesByType as t (t.key)}
							<tr><td>{t.key}</td><td class="num">{t.count}</td></tr>
						{:else}
							<tr><td colspan="2" class="empty-cell">No searches in this range.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div>
				<h3>Latency (time to the last result)</h3>
				<div class="tiles">
					<div class="tile"><span class="value small">{formatLatency(a.search.latency.p50)}</span><span class="label">median</span></div>
					<div class="tile"><span class="value small">{formatLatency(a.search.latency.p90)}</span><span class="label">90th percentile</span></div>
					<div class="tile"><span class="value small">{formatLatency(a.search.latency.p99)}</span><span class="label">99th percentile</span></div>
				</div>
			</div>
		</div>
	</section>

	<section>
		<div class="section-header"><h2>Code navigation</h2></div>
		<div class="tiles">
			<div class="tile">
				<span class="value">{share === null ? '–' : `${share}%`}</span>
				<span class="label">precise (vs search-based)</span>
			</div>
			<div class="tile">
				<span class="value">{formatCount(a.codeIntel.crossRepoEvents.summary.totalCount)}</span>
				<span class="label">cross-repository</span>
			</div>
			<div class="tile">
				<span class="value">{a.repos.preciseCodeIntelCount} / {a.repos.count}</span>
				<span class="label">repositories with precise indexes</span>
			</div>
		</div>
		<div class="grid">
			<StatChart title="Go to definition" item={a.codeIntel.definitionClicks} {grouping} />
			<StatChart title="Find references" item={a.codeIntel.referenceClicks} {grouping} />
			<StatChart title="Precise requests" item={a.codeIntel.preciseEvents} {grouping} />
			<StatChart title="Search-based requests" item={a.codeIntel.searchBasedEvents} {grouping} />
		</div>
		<div class="grid">
			<div>
				<h3>By language</h3>
				<table class="table table-bordered table-sm">
					<thead><tr><th scope="col">Language</th><th scope="col">Precision</th><th scope="col" class="num">Requests</th></tr></thead>
					<tbody>
						{#each a.codeIntelByLanguage as l (l.language + l.precision)}
							<tr><td>{l.language || 'Unknown'}</td><td>{l.precision}</td><td class="num">{l.count}</td></tr>
						{:else}
							<tr><td colspan="3" class="empty-cell">No code navigation in this range.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div>
				<h3>Top repositories</h3>
				<div class="table-scroll">
					<table class="table table-bordered table-sm">
						<thead><tr><th scope="col">Repository</th><th scope="col">Kind</th><th scope="col">Precision</th><th scope="col" class="num">Requests</th></tr></thead>
						<tbody>
							{#each a.codeIntelTopRepositories as r (r.name + r.language + r.kind + r.precision)}
								<tr>
									<td class="mono"><a href="/{r.name}">{r.name}</a>{#if r.hasPrecise}<span class="badge badge-success">precise index</span>{/if}</td>
									<td>{r.kind}</td>
									<td>{r.precision}</td>
									<td class="num">{r.events}</td>
								</tr>
							{:else}
								<tr><td colspan="4" class="empty-cell">No code navigation in this range.</td></tr>
							{/each}
						</tbody>
					</table>
				</div>
			</div>
		</div>
	</section>

	<section>
		<div class="section-header"><h2>Features</h2></div>
		<div class="grid">
			<StatChart title="Notebooks created" item={a.notebooks.creations} {grouping} />
			<StatChart title="Notebook views" item={a.notebooks.views} {grouping} />
			<StatChart title="Watches created" item={a.codeMonitors.creations} {grouping} />
			<StatChart title="Watch views" item={a.codeMonitors.views} {grouping} />
			<StatChart title="Bulk change views" item={a.batchChanges.views} {grouping} />
			<StatChart title="Changesets created" item={a.batchChanges.changesetsCreated} {grouping} showUsers={false} />
			<StatChart title="Changesets merged" item={a.batchChanges.changesetsMerged} {grouping} showUsers={false} />
			<StatChart title="Trends created" item={a.codeInsights.creations} {grouping} />
			<StatChart title="Trend views" item={a.codeInsights.views} {grouping} />
		</div>
	</section>
{:else if loading}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{/if}

<style>
	.filters {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-3);
		margin-bottom: var(--space-2);
	}
	.filters label {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		white-space: nowrap;
	}
	section .section-header {
		margin-top: var(--space-8);
	}
	h3 {
		font-size: var(--text-md);
		margin: var(--space-3) 0 var(--space-2);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(18rem, 100%), 1fr));
		gap: var(--space-4);
		margin: var(--space-4) 0;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
		gap: var(--space-3);
		margin: var(--space-3) 0;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-3) var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-surface);
		box-shadow: var(--shadow-xs);
	}
	.value {
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.value.small {
		font-size: var(--text-lg);
	}
	.label {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}
	td .badge {
		margin-left: var(--space-2);
	}
</style>
