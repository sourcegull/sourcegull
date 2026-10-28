<script lang="ts">
	import { page } from '$app/state';
	import { creationURL } from '#lib/insights/creation.ts';
	import { dashboardURL, type InsightType } from '#lib/insights/insights.ts';

	// Creation from a dashboard adds the new trend to it (`?dashboardId=`).
	const dashboardId = $derived(page.url.searchParams.get('dashboardId'));

	const CARDS: { type: InsightType; title: string; example?: string }[] = [
		{ type: 'search', title: 'Track changes', example: 'Tracking architecture, naming, or language migrations.' },
		{ type: 'capture-group', title: 'Detect and track patterns', example: 'Detecting and tracking language or package versions.' },
		{ type: 'compute', title: 'Group results', example: 'Tracking a migration by repository.' },
		{ type: 'language-stats', title: 'Language usage' }
	];
</script>

<svelte:head><title>Create trend · Trends · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb"><a href="/trends">Trends</a> / <span>Create</span></nav>
	<div class="page-header">
		<div>
			<h1>Create new trend</h1>
			<p class="page-description">Code trends analyze your code based on any search query. Pick a kind to start.</p>
		</div>
	</div>
	<ul class="cards" aria-label="Trend types">
		{#each CARDS as c (c.type)}
			<li class="card card-interactive insight-type">
				<h2>{c.title}</h2>
				<p class="muted">
					{#if c.type === 'search'}
						Trend <strong>based on a custom search query</strong> that creates visualization of the data series you will
						define <strong>manually.</strong>
					{:else if c.type === 'capture-group'}
						Trend <strong>based on a search query</strong> that automatically generates data series for each unique
						value of the <strong>regular expression capture group</strong> included in the search query. Chart will be
						updated as new values appear in the code.
					{:else if c.type === 'compute'}
						Trend based on a custom search query that <strong>groups results by</strong> repository, language, path,
						author or date.
					{:else}
						Shows language usage in your repository based on number of lines of code.
					{/if}
				</p>
				{#if c.example}
					<p class="example"><small class="muted">Example use</small><small>{c.example}</small></p>
				{/if}
				<a class="btn btn-secondary" href={creationURL(c.type, dashboardId)}>Create {c.title.toLowerCase()} trend</a>
			</li>
		{/each}
	</ul>
	<p>
		<a href={dashboardId ? dashboardURL(dashboardId) : '/trends'}>Back to {dashboardId ? 'the dashboard' : 'trends'}</a>
	</p>
</div>

<style>
	.cards {
		list-style: none;
		margin: 0 0 var(--space-6);
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
		gap: var(--space-4);
	}
	.insight-type {
		min-width: 0;
		padding: var(--space-5);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.insight-type h2 {
		margin: 0;
		font-size: var(--text-lg);
	}
	.insight-type p {
		margin: 0;
	}
	/* The labels are long ("Create detect and track patterns trend"): wrap inside the card. */
	.insight-type .btn {
		margin-top: auto;
		justify-content: center;
		text-align: center;
		white-space: normal;
		overflow-wrap: anywhere;
		max-width: 100%;
	}
	.example {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding-top: var(--space-2);
		border-top: 1px solid var(--color-border);
	}
</style>
