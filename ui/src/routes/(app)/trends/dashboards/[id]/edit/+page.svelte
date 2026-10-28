<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import DashboardForm from '#lib/insights/DashboardForm.svelte';
	import { loadDashboard, loadOwners, type DashboardOwners } from '#lib/insights/api.ts';
	import { dashboardURL, grantsInput, visibilityOf, type Visibility } from '#lib/insights/insights.ts';
	import {
		UPDATE_INSIGHTS_DASHBOARD,
		type DashboardNode,
		type PermissionGrantsInput,
		type UpdateInsightsDashboardResult
	} from '#lib/insights/queries.ts';

	const id = $derived(page.params.id ?? '');

	let owners = $state.raw<DashboardOwners | null>(null);
	let dashboard = $state.raw<DashboardNode | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(dashboardId: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const [o, d] = await Promise.all([loadOwners(), loadDashboard(dashboardId)]);
			owners = o;
			dashboard = d;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		const dashboardId = id;
		untrack(() => void load(dashboardId));
	});

	async function submit(title: string, visibility: Visibility): Promise<void> {
		if (!owners?.userId || !dashboard) return;
		const r = await graphql<
			UpdateInsightsDashboardResult,
			{ id: string; input: { title: string; grants: PermissionGrantsInput } }
		>(UPDATE_INSIGHTS_DASHBOARD, {
			id: dashboard.id,
			input: { title, grants: grantsInput(visibility, owners.userId) }
		});
		await goto(dashboardURL(r.updateInsightsDashboard.dashboard.id));
	}
</script>

<svelte:head><title>Configure {dashboard?.title ?? 'dashboard'} · Trends · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<div class="page-header">
		<div>
			<h1>Configure dashboard</h1>
			<p class="page-description">Dashboards group your trends and let you share them with others.</p>
		</div>
	</div>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{:else if !dashboard || !owners}
		<div class="empty-state"><h2>Oops, we couldn't find the dashboard</h2></div>
	{:else}
		{#key dashboard.id}
			<DashboardForm
				name={dashboard.title}
				visibility={visibilityOf(dashboard.grants, owners.orgs)}
				orgs={owners.orgs}
				submitLabel="Save changes"
				submittingLabel="Saving"
				onsubmit={submit}
				oncancel={() => history.back()}
			/>
		{/key}
	{/if}
</div>

