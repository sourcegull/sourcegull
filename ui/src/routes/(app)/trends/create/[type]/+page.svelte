<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { graphql } from '#lib/graphql.ts';
	import CaptureGroupInsightForm from '#lib/insights/CaptureGroupInsightForm.svelte';
	import ComputeInsightForm from '#lib/insights/ComputeInsightForm.svelte';
	import LangStatsInsightForm from '#lib/insights/LangStatsInsightForm.svelte';
	import SearchInsightForm from '#lib/insights/SearchInsightForm.svelte';
	import {
		captureGroupCreateInput,
		computeCreateInput,
		creationType,
		creationURL,
		langStatsCreateInput,
		searchCreateInput
	} from '#lib/insights/creation.ts';
	import { clearDraft, initialFields, saveDraft } from '#lib/insights/initial.ts';
	import { ALL_INSIGHTS, INSIGHT_TYPE_LABELS, dashboardURL } from '#lib/insights/insights.ts';
	import {
		CREATE_LINE_CHART_SEARCH_INSIGHT,
		CREATE_PIE_CHART_SEARCH_INSIGHT,
		type LineChartSearchInsightInput,
		type PieChartSearchInsightInput
	} from '#lib/insights/queries.ts';

	const type = $derived(creationType(page.params.type ?? ''));
	const rawDashboardId = $derived(page.url.searchParams.get('dashboardId'));
	// The new trend goes onto the dashboard it was created from.
	const dashboardId = $derived(rawDashboardId && rawDashboardId !== ALL_INSIGHTS ? rawDashboardId : null);
	const back = $derived(dashboardId ? dashboardURL(dashboardId) : '/trends/all');

	// URL parameters (the search results page's "Create trend"), else the saved draft.
	const params = $derived(page.url.searchParams);

	async function createLine(input: LineChartSearchInsightInput): Promise<void> {
		await graphql<unknown, { input: LineChartSearchInsightInput }>(CREATE_LINE_CHART_SEARCH_INSIGHT, { input });
		if (type) clearDraft(type);
		await goto(back);
	}

	async function createPie(input: PieChartSearchInsightInput): Promise<void> {
		await graphql<unknown, { input: PieChartSearchInsightInput }>(CREATE_PIE_CHART_SEARCH_INSIGHT, { input });
		if (type) clearDraft(type);
		await goto(back);
	}

	function cancel(): Promise<void> {
		if (type) clearDraft(type);
		return goto(back);
	}
	const props = { submitLabel: 'Create trend', submittingLabel: 'Creating' };
</script>

<svelte:head><title>Create trend · Trends · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb">
		<a href="/trends">Trends</a> / <a href={creationURL(null, dashboardId)}>Create</a> /
		<span>{type ? INSIGHT_TYPE_LABELS[type] : 'Unknown'}</span>
	</nav>
	{#if type === null}
		<div class="alert alert-danger" role="alert">
			There is no such trend type. <a href={creationURL(null, dashboardId)}>Choose one</a>.
		</div>
	{:else}
		<div class="page-header">
			<div>
				<h1>
					{#if type === 'search'}Create trend{:else if type === 'capture-group'}Create detect and track patterns trend{:else if type === 'compute'}Create group results trend{:else}Set up new language usage trend{/if}
				</h1>
				<p class="page-description">
					{#if type === 'search'}
						Search-based code trends analyze your code based on any search query.
					{:else if type === 'capture-group'}
						Capture group code trends analyze your code based on generated data series queries.
					{:else if type === 'compute'}
						Group results trends group the results of search queries by repository, language, path, author or date.
					{:else}
						Shows language usage in your repository based on number of lines of code.
					{/if}
				</p>
			</div>
		</div>
		{#key type}
			{#if type === 'search'}
				<SearchInsightForm
					initial={initialFields('search', params)}
					{...props}
					onchange={(f) => saveDraft('search', f)}
					onsubmit={(f) => createLine(searchCreateInput(f, dashboardId))}
					oncancel={cancel}
				/>
			{:else if type === 'capture-group'}
				<CaptureGroupInsightForm
					initial={initialFields('capture-group', params)}
					{...props}
					onchange={(f) => saveDraft('capture-group', f)}
					onsubmit={(f) => createLine(captureGroupCreateInput(f, dashboardId))}
					oncancel={cancel}
				/>
			{:else if type === 'compute'}
				<ComputeInsightForm
					initial={initialFields('compute', params)}
					{...props}
					onchange={(f) => saveDraft('compute', f)}
					onsubmit={(f) => createLine(computeCreateInput(f, dashboardId))}
					oncancel={cancel}
				/>
			{:else}
				<LangStatsInsightForm
					initial={initialFields('language-stats', params)}
					{...props}
					onchange={(f) => saveDraft('language-stats', f)}
					onsubmit={(f) => createPie(langStatsCreateInput(f, dashboardId))}
					oncancel={cancel}
				/>
			{/if}
		{/key}
	{/if}
</div>

