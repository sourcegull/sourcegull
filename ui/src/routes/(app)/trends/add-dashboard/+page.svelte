<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import DashboardForm from '#lib/insights/DashboardForm.svelte';
	import { loadOwners, type DashboardOwners } from '#lib/insights/api.ts';
	import { dashboardURL, grantsInput, type Visibility } from '#lib/insights/insights.ts';
	import {
		CREATE_INSIGHTS_DASHBOARD,
		type CreateInsightsDashboardResult,
		type PermissionGrantsInput
	} from '#lib/insights/queries.ts';

	let owners = $state.raw<DashboardOwners | null>(null);
	let error = $state<string | null>(null);

	onMount(async () => {
		try {
			owners = await loadOwners();
		} catch (err) {
			error = errorMessage(err);
		}
	});

	async function submit(title: string, visibility: Visibility): Promise<void> {
		if (!owners?.userId) throw new Error('You have to be signed in to add a dashboard.');
		const r = await graphql<CreateInsightsDashboardResult, { input: { title: string; grants: PermissionGrantsInput } }>(
			CREATE_INSIGHTS_DASHBOARD,
			{ input: { title, grants: grantsInput(visibility, owners.userId) } }
		);
		await goto(dashboardURL(r.createInsightsDashboard.dashboard.id));
	}
</script>

<svelte:head><title>Add dashboard · Trends · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<div class="page-header">
		<div>
			<h1>Add new dashboard</h1>
			<p class="page-description">Dashboards group your trends and let you share them with others.</p>
		</div>
	</div>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if owners}
		<DashboardForm
			orgs={owners.orgs}
			submitLabel="Add dashboard"
			submittingLabel="Adding"
			onsubmit={submit}
			oncancel={() => history.back()}
		/>
	{:else}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{/if}
</div>

