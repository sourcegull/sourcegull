<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { errorMessage } from '#lib/graphql.ts';
	import { loadDashboards } from '#lib/insights/api.ts';
	import { dashboardURL, lastVisitedDashboard } from '#lib/insights/insights.ts';

	let error = $state<string | null>(null);

	// The last dashboard opened, else the dashboards tab, else "Getting started".
	onMount(async () => {
		try {
			const dashboards = await loadDashboards();
			const last = lastVisitedDashboard();
			if (last && dashboards.some((d) => d.id === last)) await goto(dashboardURL(last), { replaceState: true });
			else if (dashboards.length > 0) await goto('/trends/dashboards', { replaceState: true });
			else await goto('/trends/about', { replaceState: true });
		} catch (err) {
			error = errorMessage(err);
		}
	});
</script>

<svelte:head><title>Trends · Sourcegull</title></svelte:head>

<div class="page insights-page">
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{/if}
</div>

