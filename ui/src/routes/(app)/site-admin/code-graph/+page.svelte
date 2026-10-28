<script lang="ts">
	import PoliciesView from '#lib/codeintel/PoliciesView.svelte';
	import RankingStatus from '#lib/codeintel/RankingStatus.svelte';
</script>

<svelte:head><title>Code graph policies · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Code graph</h1>
		<p class="page-description">
			Which commits keep precise indexes and for how long, and the global ranking of files used to
			order search results. <a href="/site-admin/code-graph/coverage">Index coverage</a> shows which
			repositories have a current precise index.
		</p>
	</div>
</div>

<div class="card ranking-card">
	<RankingStatus />
</div>

<PoliciesView />

<style>
	.ranking-card {
		padding: var(--space-3) var(--space-5);
		margin-bottom: var(--space-6);
	}
	.ranking-card :global(.ranking) {
		margin: 0;
	}
</style>
