<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount, untrack } from 'svelte';
	import { logEvent } from '#lib/events.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import AddInsightsDialog from '#lib/insights/AddInsightsDialog.svelte';
	import DashboardSelect from '#lib/insights/DashboardSelect.svelte';
	import InsightCard from '#lib/insights/InsightCard.svelte';
	import InsightsHeader from '#lib/insights/InsightsHeader.svelte';
	import { loadDashboard, loadDashboards, loadOwners, viewIdsOf } from '#lib/insights/api.ts';
	import {
		dashboardURL,
		groupDashboards,
		lastVisitedDashboard,
		rememberDashboard,
		withDashboardParam
	} from '#lib/insights/insights.ts';
	import { DELETE_INSIGHTS_DASHBOARD, type DashboardNode, type OrgRef } from '#lib/insights/queries.ts';

	const id = $derived(page.params.id ?? null);

	let dashboards = $state.raw<DashboardNode[]>([]);
	let orgs = $state.raw<OrgRef[]>([]);
	let listLoaded = $state(false);
	let current = $state.raw<DashboardNode | null>(null);
	let viewIds = $state.raw<string[]>([]);
	let loading = $state(true);
	let notFound = $state(false);
	let error = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let menuOpen = $state(false);
	let addOpen = $state(false);
	let copied = $state(false);
	let menuRoot = $state<HTMLDivElement>();
	let seq = 0;

	async function loadList(): Promise<void> {
		try {
			const [list, owners] = await Promise.all([loadDashboards(), loadOwners()]);
			dashboards = list;
			orgs = owners.orgs;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			listLoaded = true;
		}
	}

	async function loadCurrent(dashboardId: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		notFound = false;
		try {
			const d = await loadDashboard(dashboardId);
			if (mine !== seq) return;
			current = d;
			viewIds = d ? viewIdsOf(d) : [];
			notFound = d === null;
			if (d) rememberDashboard(d.id);
		} catch (err) {
			if (mine === seq) {
				current = null;
				error = errorMessage(err);
			}
		} finally {
			if (mine === seq) loading = false;
		}
	}

	onMount(() => {
		logEvent('ViewInsights');
		void loadList();
	});

	// A dashboard id loads it; none picks the last opened dashboard or the first one listed.
	$effect(() => {
		const dashboardId = id;
		untrack(() => {
			menuOpen = false;
			addOpen = false;
			if (dashboardId) {
				void loadCurrent(dashboardId);
			} else {
				current = null;
				viewIds = [];
			}
		});
	});

	$effect(() => {
		if (id || !listLoaded) return;
		untrack(() => {
			const last = lastVisitedDashboard();
			const first = groupDashboards(dashboards, orgs)[0]?.dashboards[0];
			const target = dashboards.find((d) => d.id === last) ?? first;
			if (target) void goto(dashboardURL(target.id), { replaceState: true });
			else loading = false;
		});
	});

	async function deleteDashboard(): Promise<void> {
		menuOpen = false;
		if (!current) return;
		if (
			!confirm(
				`Delete “${current.title}”? This can't be undone. You will still be able to access trends from this dashboard in “All trends”.`
			)
		) {
			return;
		}
		actionError = null;
		try {
			await graphql<unknown, { id: string }>(DELETE_INSIGHTS_DASHBOARD, { id: current.id });
			rememberDashboard(null);
			dashboards = dashboards.filter((d) => d.id !== current?.id);
			await goto('/trends/dashboards');
		} catch (err) {
			actionError = errorMessage(err);
		}
	}

	async function copyLink(): Promise<void> {
		menuOpen = false;
		try {
			await navigator.clipboard.writeText(location.href);
			copied = true;
			setTimeout(() => (copied = false), 2000);
		} catch (err) {
			actionError = `Copy failed: ${errorMessage(err)}`;
		}
	}

	function onwindowclick(e: MouseEvent): void {
		if (menuOpen && menuRoot && !e.composedPath().includes(menuRoot)) menuOpen = false;
	}

	function dropView(viewId: string): void {
		viewIds = viewIds.filter((v) => v !== viewId);
	}

	function reload(): void {
		if (id) void loadCurrent(id);
	}
</script>

<svelte:head><title>{current ? `${current.title} · ` : ''}Trends · Sourcegull</title></svelte:head>
<svelte:window onclick={onwindowclick} />

<div class="page insights-page">
	<InsightsHeader tab="dashboards" dashboardId={id} />

	<div class="dashboard-header">
		<span class="select-label">Dashboard:</span>
		<DashboardSelect {current} {dashboards} {orgs} />
		<div class="menu-wrap" bind:this={menuRoot}>
			<button
				type="button"
				class="btn icon"
				aria-label="Dashboard options"
				aria-haspopup="menu"
				aria-expanded={menuOpen}
				onclick={() => (menuOpen = !menuOpen)}>⋮</button
			>
			{#if menuOpen}
				<div class="menu popover-surface" role="menu" aria-label="Dashboard options">
					<a
						role="menuitem"
						href={current ? `${dashboardURL(current.id)}/edit` : undefined}
						aria-disabled={!current}
						title={current ? undefined : 'Dashboard not found'}>Configure dashboard</a
					>
					<button role="menuitem" type="button" onclick={copyLink}>Copy link</button>
					<hr />
					<button role="menuitem" type="button" class="danger" disabled={!current} onclick={deleteDashboard}
						>Delete</button
					>
				</div>
			{/if}
		</div>
		{#if copied}<span class="muted" role="status">Copied!</span>{/if}
		<span class="spacer"></span>
		<button
			type="button"
			class="btn"
			disabled={!current}
			title={current ? undefined : 'Dashboard not found'}
			onclick={() => (addOpen = true)}>Add or remove trends</button
		>
	</div>

	{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	{#if current}
		{#if viewIds.length === 0}
			<section class="empty-dashboard">
				<button type="button" class="add-card card" data-testid="add-insights-button-card" onclick={() => (addOpen = true)}>
					<span class="plus" aria-hidden="true"><Icon name="plus" size={28} /></span>
					<span>Add trends</span>
				</button>
				<a href={withDashboardParam('/trends/create', current.id)}>or, create new trend</a>
			</section>
		{:else}
			<div class="grid" role="list" aria-label="Trends">
				{#each viewIds as viewId (viewId)}
					<div role="listitem" class="cell">
						<InsightCard
							id={viewId}
							dashboard={current}
							onremoved={dropView}
							ondeleted={dropView}
							onviewcreated={reload}
						/>
					</div>
				{/each}
			</div>
		{/if}
	{:else if loading && !error}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{:else if listLoaded && dashboards.length === 0 && !id}
		<section class="empty-state">
			<span class="icon"><Icon name="insights" size={32} /></span>
			<h2>Your dashboard will appear here</h2>
			<p>Your instance does not have any dashboards or you may not have permissions to view them.</p>
			<a class="btn btn-primary" href="/trends/add-dashboard"><Icon name="plus" size={16} />Create your first dashboard</a>
		</section>
	{:else if notFound}
		<section class="empty-state">
			<h2>Hmm, the dashboard wasn't found.</h2>
			<a class="btn" href="/trends/dashboards">All dashboards</a>
		</section>
	{/if}
</div>

{#if addOpen && current}
	<AddInsightsDialog
		dashboard={current}
		{viewIds}
		onclose={() => (addOpen = false)}
		onsaved={() => {
			addOpen = false;
			reload();
		}}
	/>
{/if}

<style>
	.insights-page {
		max-width: 90rem;
	}
	.dashboard-header {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-bottom: var(--space-4);
	}
	.select-label {
		font-weight: 600;
	}
	.spacer {
		flex: 1;
	}
	.icon {
		padding: 0.4rem 0.6rem;
		line-height: 1;
	}
	.menu-wrap {
		position: relative;
	}
	.menu {
		position: absolute;
		left: 0;
		top: calc(100% + var(--space-1));
		z-index: 20;
		min-width: 13rem;
		padding: var(--space-1) 0;
		display: flex;
		flex-direction: column;
		box-shadow: var(--shadow-lg);
	}
	.menu a,
	.menu button {
		display: block;
		padding: var(--space-1) var(--space-3);
		border: 0;
		background: none;
		color: var(--color-text);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.menu a:hover,
	.menu button:hover:not(:disabled) {
		background: var(--color-bg-hover);
		text-decoration: none;
	}
	.menu button:disabled,
	.menu a[aria-disabled='true'] {
		opacity: 0.55;
		pointer-events: none;
	}
	.menu .danger {
		color: var(--color-danger);
	}
	.menu hr {
		border: 0;
		border-top: 1px solid var(--color-border);
		margin: var(--space-1) 0;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 26rem), 1fr));
		gap: var(--space-4);
	}
	.cell {
		display: flex;
		min-width: 0;
	}
	.cell > :global(*) {
		flex: 1;
	}
	.empty-dashboard {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-3);
	}
	.add-card {
		width: 100%;
		min-height: 14rem;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: var(--space-2);
		cursor: pointer;
		color: var(--color-text-muted);
		font: inherit;
		border-style: dashed;
	}
	.add-card:hover {
		background: var(--color-bg-hover);
		border-color: var(--color-primary);
		color: var(--color-text);
	}
	.plus {
		display: inline-flex;
		color: var(--color-text-subtle);
	}
</style>
