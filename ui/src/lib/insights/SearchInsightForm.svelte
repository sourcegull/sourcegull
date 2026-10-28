<!--
	The search based trend wizard ("Track changes"): repositories, data
	series (name, color, query each), title and granularity, with a live preview of the series
	over the chosen repositories.
-->
<script lang="ts">
	import { graphql } from '#lib/graphql.ts';
	import CreationForm from './CreationForm.svelte';
	import LivePreview from './LivePreview.svelte';
	import RepoScopeSection from './RepoScopeSection.svelte';
	import SeriesEditor from './SeriesEditor.svelte';
	import StepPicker from './StepPicker.svelte';
	import TitleField from './TitleField.svelte';
	import {
		defaultSearchFields,
		isSeriesValid,
		previewLineSeries,
		repoFieldsError,
		searchFormErrors,
		searchPreviewInput,
		stepValueError,
		type PreviewModel,
		type SearchInsightFields
	} from './creation.ts';
	import { INSIGHT_PREVIEW, type InsightPreviewInput, type InsightPreviewResult } from './queries.ts';

	interface Props {
		initial: SearchInsightFields;
		edit?: boolean;
		submitLabel: string;
		submittingLabel: string;
		onsubmit: (fields: SearchInsightFields) => Promise<void>;
		oncancel: () => void;
		/** Called with the fields after each edit (the creation page saves them as a draft). */
		onchange?: (fields: SearchInsightFields) => void;
	}

	let { initial, edit = false, submitLabel, submittingLabel, onsubmit, oncancel, onchange }: Props = $props();

	// svelte-ignore state_referenced_locally
	let f = $state<SearchInsightFields>(structuredClone(initial));

	// Edits after the first render go to `onchange`.
	let rendered = false;
	$effect(() => {
		const fields = $state.snapshot(f);
		if (rendered) onchange?.(fields);
		rendered = true;
	});
	let submitted = $state(false);
	let queryError = $state<string | null>(null);
	let checking = $state(false);

	const errors = $derived(searchFormErrors(f));
	const repoValid = $derived(
		repoFieldsError(f) === null && (f.repoMode !== 'search-query' || (queryError === null && !checking))
	);
	const valid = $derived(Object.keys(errors).length === 0 && repoValid);
	const previewReady = $derived(repoValid && f.series.some(isSeriesValid) && stepValueError(f.stepValue) === null);
	const previewInput = $derived(searchPreviewInput(f));

	async function loadPreview(signal: AbortSignal): Promise<PreviewModel> {
		const input = previewInput;
		const r = await graphql<InsightPreviewResult, { input: InsightPreviewInput }>(INSIGHT_PREVIEW, { input }, { signal });
		const strokes = f.series.filter(isSeriesValid).map((s) => ({ label: s.name.trim(), stroke: s.stroke }));
		return { kind: 'line', series: previewLineSeries(r.searchInsightPreview, strokes) };
	}

	function clear(): void {
		f = defaultSearchFields();
		submitted = false;
	}
</script>

<CreationForm
	label="Track changes trend creation form"
	{valid}
	bind:submitted
	{submitLabel}
	{submittingLabel}
	onsubmit={() => onsubmit($state.snapshot(f))}
	{oncancel}
	onclear={edit ? undefined : clear}
>
	{#snippet fields()}
		<RepoScopeSection
			bind:repoMode={f.repoMode}
			bind:repoQuery={f.repoQuery}
			bind:repositories={f.repositories}
			bind:queryError
			bind:checking
			{submitted}
		/>
		<SeriesEditor bind:series={f.series} {submitted} />
		<section class="settings">
			<h2>Chart settings</h2>
			<TitleField bind:title={f.title} {submitted} />
			<StepPicker bind:stepValue={f.stepValue} bind:stepUnit={f.stepUnit} {submitted} />
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
	.settings {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	h2 {
		margin: 0;
		font-size: 1.05rem;
	}
</style>
