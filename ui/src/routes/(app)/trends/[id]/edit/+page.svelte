<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import CaptureGroupInsightForm from '#lib/insights/CaptureGroupInsightForm.svelte';
	import ComputeInsightForm from '#lib/insights/ComputeInsightForm.svelte';
	import LangStatsInsightForm from '#lib/insights/LangStatsInsightForm.svelte';
	import SearchInsightForm from '#lib/insights/SearchInsightForm.svelte';
	import {
		captureGroupUpdateInput,
		computeUpdateInput,
		editFormOf,
		langStatsUpdateInput,
		searchUpdateInput,
		viewControlsOf,
		type EditForm
	} from '#lib/insights/creation.ts';
	import { ALL_INSIGHTS, INSIGHT_TYPE_LABELS, dashboardURL, insightURL } from '#lib/insights/insights.ts';
	import {
		INSIGHT_VIEW,
		UPDATE_LINE_CHART_SEARCH_INSIGHT,
		UPDATE_PIE_CHART_SEARCH_INSIGHT,
		type InsightViewNode,
		type InsightViewResult,
		type InsightViewVariables,
		type UpdateLineChartSearchInsightInput,
		type UpdatePieChartSearchInsightInput
	} from '#lib/insights/queries.ts';

	const id = $derived(page.params.id ?? '');
	const rawDashboardId = $derived(page.url.searchParams.get('dashboardId'));
	const dashboardId = $derived(rawDashboardId && rawDashboardId !== ALL_INSIGHTS ? rawDashboardId : null);
	const back = $derived(
		rawDashboardId === ALL_INSIGHTS ? '/trends/all' : dashboardId ? dashboardURL(dashboardId) : insightURL(id)
	);

	let view = $state.raw<InsightViewNode | null>(null);
	let form = $state.raw<EditForm | null>(null);
	let loadError = $state<string | null>(null);
	let missing = $state(false);

	onMount(async () => {
		try {
			const r = await graphql<InsightViewResult, InsightViewVariables>(INSIGHT_VIEW, { id });
			const v = r.insightViews.nodes[0] ?? null;
			if (!v) {
				missing = true;
				return;
			}
			view = v;
			form = editFormOf(v);
		} catch (err) {
			loadError = errorMessage(err);
		}
	});

	async function updateLine(input: UpdateLineChartSearchInsightInput): Promise<void> {
		await graphql<unknown, { id: string; input: UpdateLineChartSearchInsightInput }>(UPDATE_LINE_CHART_SEARCH_INSIGHT, {
			id,
			input
		});
		await goto(back);
	}

	async function updatePie(input: UpdatePieChartSearchInsightInput): Promise<void> {
		await graphql<unknown, { id: string; input: UpdatePieChartSearchInsightInput }>(UPDATE_PIE_CHART_SEARCH_INSIGHT, {
			id,
			input
		});
		await goto(back);
	}

	const cancel = (): Promise<void> => goto(back);
	const props = { edit: true, submitLabel: 'Save changes', submittingLabel: 'Saving' };
</script>

<svelte:head><title>Edit trend · Trends · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb">
		<a href="/trends">Trends</a> / <a href={insightURL(id)}>Trend</a> / <span>Edit</span>
	</nav>
	{#if missing}
		<div class="alert alert-danger" role="alert">
			We couldn't find that trend. It may not exist, or you may not have access to it.
		</div>
		<p><a href={dashboardId ? dashboardURL(dashboardId) : '/trends'}>Back to trends</a></p>
	{:else if loadError}
		<div class="alert alert-danger" role="alert">{loadError}</div>
	{:else if view && form}
		{@const controls = viewControlsOf(view)}
		<div class="page-header">
			<div>
				<h1>Edit trend</h1>
				<p class="page-description">{INSIGHT_TYPE_LABELS[form.type]} trend</p>
			</div>
		</div>
		{#if view.dashboardReferenceCount > 1}
			<div class="alert alert-info" role="status">
				This trend is included in {view.dashboardReferenceCount} dashboards. Changes will apply to all of them.
			</div>
		{/if}
		{#if form.type === 'search'}
			<SearchInsightForm
				initial={form.fields}
				{...props}
				onsubmit={(f) => updateLine(searchUpdateInput(f, controls))}
				oncancel={cancel}
			/>
		{:else if form.type === 'capture-group'}
			{@const seriesId = form.seriesId}
			<CaptureGroupInsightForm
				initial={form.fields}
				{...props}
				onsubmit={(f) => updateLine(captureGroupUpdateInput(f, seriesId, controls))}
				oncancel={cancel}
			/>
		{:else if form.type === 'compute'}
			<ComputeInsightForm
				initial={form.fields}
				{...props}
				onsubmit={(f) => updateLine(computeUpdateInput(f, controls))}
				oncancel={cancel}
			/>
		{:else}
			<LangStatsInsightForm
				initial={form.fields}
				{...props}
				onsubmit={(f) => updatePie(langStatsUpdateInput(f))}
				oncancel={cancel}
			/>
		{/if}
	{:else}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{/if}
</div>

