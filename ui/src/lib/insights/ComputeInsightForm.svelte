<!--
	The group results trend wizard (`search-compute`): a list of repositories, data series,
	what to group the results by (repository, language, path, author or date) and a title, with a
	live preview of the bar chart.
-->
<script lang="ts">
	import { graphql } from '#lib/graphql.ts';
	import CreationForm from './CreationForm.svelte';
	import LivePreview from './LivePreview.svelte';
	import RepositoriesInput from './RepositoriesInput.svelte';
	import SeriesEditor from './SeriesEditor.svelte';
	import TitleField from './TitleField.svelte';
	import { seriesColor } from './charts.ts';
	import {
		GROUP_BY_OPTIONS,
		computeFormErrors,
		computePreviewInput,
		defaultComputeFields,
		groupByUnavailable,
		isSeriesValid,
		previewBarData,
		repositoriesError,
		type ComputeFields,
		type PreviewModel
	} from './creation.ts';
	import { INSIGHT_PREVIEW, type InsightPreviewInput, type InsightPreviewResult } from './queries.ts';

	interface Props {
		initial: ComputeFields;
		edit?: boolean;
		submitLabel: string;
		submittingLabel: string;
		onsubmit: (fields: ComputeFields) => Promise<void>;
		oncancel: () => void;
		/** Called with the fields after each edit (the creation page saves them as a draft). */
		onchange?: (fields: ComputeFields) => void;
	}

	let { initial, edit = false, submitLabel, submittingLabel, onsubmit, oncancel, onchange }: Props = $props();

	// svelte-ignore state_referenced_locally
	let f = $state<ComputeFields>(structuredClone(initial));

	// Edits after the first render go to `onchange`.
	let rendered = false;
	$effect(() => {
		const fields = $state.snapshot(f);
		if (rendered) onchange?.(fields);
		rendered = true;
	});
	let submitted = $state(false);
	let reposTouched = $state(false);

	const errors = $derived(computeFormErrors(f));
	const valid = $derived(Object.keys(errors).length === 0);
	const reposError = $derived((reposTouched || submitted) && repositoriesError(f.repositories));
	const previewReady = $derived(repositoriesError(f.repositories) === null && f.series.some(isSeriesValid));
	const previewInput = $derived(computePreviewInput(f));

	// A group the series cannot use falls back to the repository.
	$effect(() => {
		if (groupByUnavailable(f.groupBy, f.series)) f.groupBy = 'REPO';
	});

	async function loadPreview(signal: AbortSignal): Promise<PreviewModel> {
		const r = await graphql<InsightPreviewResult, { input: InsightPreviewInput }>(
			INSIGHT_PREVIEW,
			{ input: previewInput },
			{ signal }
		);
		return { kind: 'bar', data: previewBarData(r.searchInsightPreview, f.series[0]?.stroke || seriesColor(0)) };
	}

	function clear(): void {
		f = defaultComputeFields();
		submitted = false;
		reposTouched = false;
	}
</script>

<CreationForm
	label="Group results trend creation form"
	{valid}
	bind:submitted
	{submitLabel}
	{submittingLabel}
	onsubmit={() => onsubmit($state.snapshot(f))}
	{oncancel}
	onclear={edit ? undefined : clear}
>
	{#snippet fields()}
		<section class="section">
			<h2>Targeted repositories</h2>
			<p class="muted subtitle">Create a list of repositories to run your search over</p>
			<RepositoriesInput
				id="insight-repositories"
				bind:value={f.repositories}
				invalid={!!reposError}
				describedby="insight-repositories-hint"
				onblur={() => (reposTouched = true)}
			/>
			{#if reposError}
				<span class="error" id="insight-repositories-hint" role="alert">{reposError}</span>
			{:else}
				<span class="hint muted" id="insight-repositories-hint">Find and choose at least 1 repository to run trend</span>
			{/if}
		</section>
		<SeriesEditor bind:series={f.series} {submitted} />
		<section class="section">
			<h2 id="group-by-label">Map result</h2>
			<div class="group-by" role="radiogroup" aria-labelledby="group-by-label">
				{#each GROUP_BY_OPTIONS as o (o.value)}
					{@const why = groupByUnavailable(o.value, f.series)}
					<button
						type="button"
						role="radio"
						class="btn btn-sm"
						class:active={f.groupBy === o.value}
						aria-checked={f.groupBy === o.value}
						disabled={!!why}
						title={why ?? undefined}
						onclick={() => (f.groupBy = o.value)}>{o.label}</button
					>
				{/each}
			</div>
		</section>
		<section class="section">
			<h2>Chart settings</h2>
			<TitleField bind:title={f.title} {submitted} />
		</section>
	{/snippet}
	{#snippet preview()}
		<LivePreview
			ready={previewReady}
			request={JSON.stringify(previewInput)}
			load={loadPreview}
			blankMessage="The chart preview will be shown here once you have filled out the repositories and series fields."
		/>
	{/snippet}
</CreationForm>

<style>
	.section {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	h2 {
		margin: 0;
		font-size: 1.05rem;
	}
	.subtitle {
		margin: 0;
		font-size: 0.9rem;
	}
	.group-by {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.group-by .active {
		background: var(--color-primary);
		border-color: var(--color-primary);
		color: var(--color-primary-text);
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
	.hint {
		font-size: 0.85rem;
	}
</style>
