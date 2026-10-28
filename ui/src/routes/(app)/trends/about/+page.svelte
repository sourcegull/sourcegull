<script lang="ts">
	import { page } from '$app/state';
	import InsightsHeader from '#lib/insights/InsightsHeader.svelte';
	import { withDashboardParam } from '#lib/insights/insights.ts';

	const dashboardId = $derived(page.url.searchParams.get('dashboardId'));
</script>

<svelte:head><title>Getting started · Trends · Sourcegull</title></svelte:head>

<div class="page insights-page">
	<InsightsHeader tab="about" {dashboardId} />

	<section class="intro">
		<h2>Track what matters in your code</h2>
		<p>
			Code trends turn searches into charts over time: count a pattern across repositories, follow a
			migration, watch versions or licenses spread, and see which languages your code is written in.
		</p>
	</section>

	<div class="cards">
		<section class="card">
			<h3>Search based trends</h3>
			<p class="muted">One line per search query, counted at each point in time across the repositories you choose.</p>
		</section>
		<section class="card">
			<h3>Detect and track patterns</h3>
			<p class="muted">One line per value a regular expression capture group finds, such as versions or licenses.</p>
		</section>
		<section class="card">
			<h3>Group results</h3>
			<p class="muted">The results of a search grouped by repository, language, path, author or date.</p>
		</section>
		<section class="card">
			<h3>Language usage</h3>
			<p class="muted">A pie chart of the languages of the repositories you choose.</p>
		</section>
	</div>

	<section class="next">
		<h2>Dashboards</h2>
		<p>
			Dashboards group trends and share them: keep one private, share it with an organization, or make it
			visible to everyone on this instance. A trend can be on several dashboards; each card has filters
			to narrow it to some repositories or a search context, and to sort and limit its series.
		</p>
		<div class="toolbar">
			<a class="btn btn-primary" href={withDashboardParam('/trends/create', dashboardId)}>Create trend</a>
			<a class="btn" href="/trends/add-dashboard">Add dashboard</a>
		</div>
	</section>
</div>

<style>
	.cards {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 14rem), 1fr));
		gap: var(--space-3);
		margin: var(--space-4) 0;
	}
	.cards .card {
		padding: var(--space-4);
	}
	.cards h3 {
		margin: 0 0 var(--space-2);
		font-size: var(--text-md);
	}
	.cards p {
		margin: 0;
	}
</style>
