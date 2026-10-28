<!--
	The capture group trend wizard ("Detect and track"): repositories, one
	search query whose regular expression capture group generates a series per captured value
	(with a checklist of what the query may not contain), title and granularity, and
	a live preview.
-->
<script lang="ts">
	import { graphql } from '#lib/graphql.ts';
	import CreationForm from './CreationForm.svelte';
	import LivePreview from './LivePreview.svelte';
	import RepoScopeSection from './RepoScopeSection.svelte';
	import StepPicker from './StepPicker.svelte';
	import TitleField from './TitleField.svelte';
	import {
		captureGroupFormErrors,
		captureGroupPreviewInput,
		captureGroupQueryError,
		defaultCaptureGroupFields,
		previewLineSeries,
		queryChecks,
		repoFieldsError,
		stepValueError,
		type CaptureGroupFields,
		type PreviewModel
	} from './creation.ts';
	import { MAX_NUMBER_OF_SERIES } from './insights.ts';
	import { INSIGHT_PREVIEW, type InsightPreviewInput, type InsightPreviewResult } from './queries.ts';

	interface Props {
		initial: CaptureGroupFields;
		edit?: boolean;
		submitLabel: string;
		submittingLabel: string;
		onsubmit: (fields: CaptureGroupFields) => Promise<void>;
		oncancel: () => void;
		/** Called with the fields after each edit (the creation page saves them as a draft). */
		onchange?: (fields: CaptureGroupFields) => void;
	}

	let { initial, edit = false, submitLabel, submittingLabel, onsubmit, oncancel, onchange }: Props = $props();

	// svelte-ignore state_referenced_locally
	let f = $state<CaptureGroupFields>(structuredClone(initial));

	// Edits after the first render go to `onchange`.
	let rendered = false;
	$effect(() => {
		const fields = $state.snapshot(f);
		if (rendered) onchange?.(fields);
		rendered = true;
	});
	let submitted = $state(false);
	let queryTouched = $state(false);
	let queryError = $state<string | null>(null);
	let checking = $state(false);

	const errors = $derived(captureGroupFormErrors(f));
	const repoValid = $derived(
		repoFieldsError(f) === null && (f.repoMode !== 'search-query' || (queryError === null && !checking))
	);
	const valid = $derived(Object.keys(errors).length === 0 && repoValid);
	const checks = $derived(queryChecks(f.query));
	const seriesError = $derived((queryTouched || submitted) && captureGroupQueryError(f.query));
	const previewReady = $derived(
		repoValid && captureGroupQueryError(f.query) === null && stepValueError(f.stepValue) === null
	);
	const previewInput = $derived(captureGroupPreviewInput(f));

	const CHECKS: { key: keyof typeof checks; text: string; error: string }[] = [
		{
			key: 'hasCaptureGroup',
			text: 'Contains a regular expression with at least one capture group',
			error: 'should contain a regular expression with a capture group'
		},
		{
			key: 'isValidOperator',
			text: 'Does not contain boolean operators AND, OR, and NOT (regular expression boolean operators can still be used)',
			error: "shouldn't contain boolean operators, AND, OR, NOT"
		},
		{
			key: 'isValidPatternType',
			text: 'Does not contain a patternType:keyword, standard, literal, or structural',
			error: "shouldn't contain 'keyword', 'standard', 'literal', or 'structural' patterntype"
		},
		{ key: 'isNotRepo', text: 'Does not contain repo: filter', error: "shouldn't contain repo filter" },
		{ key: 'isNotContext', text: 'Does not contain context: filter', error: "shouldn't contain context filter" },
		{ key: 'isNotRev', text: 'Does not contain rev: filter', error: "shouldn't contain rev filter" },
		{
			key: 'isNotCommitOrDiff',
			text: 'Does not contain commit or diff search',
			error: "shouldn't contain commit or diff search"
		}
	];

	async function loadPreview(signal: AbortSignal): Promise<PreviewModel> {
		const r = await graphql<InsightPreviewResult, { input: InsightPreviewInput }>(
			INSIGHT_PREVIEW,
			{ input: previewInput },
			{ signal }
		);
		return { kind: 'line', series: previewLineSeries(r.searchInsightPreview, [], MAX_NUMBER_OF_SERIES) };
	}

	function clear(): void {
		f = defaultCaptureGroupFields();
		submitted = false;
		queryTouched = false;
	}
</script>

<CreationForm
	label="Detect and track trend creation form"
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
		<section class="section">
			<h2>Data series</h2>
			<p class="muted subtitle">Generated dynamically for each unique value from the regular expression capture group.</p>
			<div class="card series">
				<div class="field">
					<label for="capture-group-query">Search query</label>
					<span class="hint muted">
						Search query must contain a properly formatted regular expression with at least one capture group. The
						capture group cannot match file or repository names, it can match only the file contents.
					</span>
					<input
						id="capture-group-query"
						class="input mono"
						required
						placeholder={'Example: file:\\.pom$ <java\\.version>(.*)</java\\.version>'}
						aria-invalid={seriesError ? 'true' : undefined}
						bind:value={f.query}
						onblur={() => (queryTouched = true)}
					/>
					{#if seriesError}<span class="error" role="alert">{seriesError}</span>{/if}
				</div>
				<ul class="checks" aria-label="Search query validation checks list">
					{#each CHECKS as c (c.key)}
						{@const ok = checks[c.key]}
						<li class:valid={ok === true} class:invalid={ok === false}>
							<span class="mark" aria-hidden="true">{ok === true ? '✓' : ok === false ? '✕' : '○'}</span>
							<span>{c.text}</span>
							{#if ok === false}<span class="visually-hidden">Failed validation check. {c.error}</span>{/if}
						</li>
					{/each}
				</ul>
				<p class="info">
					<strong>Name</strong> and <strong>color</strong> of each data series will be generated automatically. Chart
					will display <strong>up to {MAX_NUMBER_OF_SERIES}</strong> data series.
				</p>
			</div>
		</section>
		<section class="section">
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
			blankMessage="The chart preview will be shown here once you have filled out the repositories and the search query fields."
		/>
	{/snippet}
</CreationForm>

<style>
	.section {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	h2 {
		margin: 0;
		font-size: 1.05rem;
	}
	.subtitle {
		margin: 0;
		font-size: 0.9rem;
	}
	.series {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	.checks {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		font-size: 0.85rem;
		color: var(--color-text-muted);
	}
	.checks li {
		display: flex;
		gap: var(--space-2);
	}
	.mark {
		width: 1rem;
		flex: none;
		text-align: center;
	}
	.valid .mark {
		color: var(--color-success);
	}
	.invalid .mark {
		color: var(--color-danger);
	}
	.info {
		margin: 0;
		padding: var(--space-2) var(--space-3);
		border-radius: 6px;
		background: var(--color-info-bg);
		font-size: 0.85rem;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
	.hint {
		font-size: 0.85rem;
	}
</style>
