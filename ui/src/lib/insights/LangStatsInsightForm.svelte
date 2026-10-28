<!--
	The language usage trend wizard (`language-stats`): one repository (checked to
	exist), a title and the threshold under which languages are grouped as "Other", with a live
	preview of the pie.
-->
<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { REPOSITORY_NAMES, type RepositoryNamesResult } from '#lib/search/queries.ts';
	import CreationForm from './CreationForm.svelte';
	import LivePreview from './LivePreview.svelte';
	import TitleField from './TitleField.svelte';
	import {
		defaultLangStatsFields,
		langStatsFormErrors,
		previewPieData,
		repositoryError,
		repositoryNotFound,
		thresholdError,
		type LangStatsFields,
		type PreviewModel
	} from './creation.ts';
	import {
		CHECK_REPOSITORY_EXISTS,
		LANGUAGE_STATISTICS_PREVIEW,
		type CheckRepositoryExistsResult,
		type LanguageStatisticsPreviewResult
	} from './queries.ts';

	interface Props {
		initial: LangStatsFields;
		edit?: boolean;
		submitLabel: string;
		submittingLabel: string;
		onsubmit: (fields: LangStatsFields) => Promise<void>;
		oncancel: () => void;
		/** Called with the fields after each edit (the creation page saves them as a draft). */
		onchange?: (fields: LangStatsFields) => void;
	}

	let { initial, edit = false, submitLabel, submittingLabel, onsubmit, oncancel, onchange }: Props = $props();

	// svelte-ignore state_referenced_locally
	let f = $state<LangStatsFields>(structuredClone(initial));

	// Edits after the first render go to `onchange`.
	let rendered = false;
	$effect(() => {
		const fields = $state.snapshot(f);
		if (rendered) onchange?.(fields);
		rendered = true;
	});
	let submitted = $state(false);
	let touched = $state<{ repository?: boolean; threshold?: boolean }>({});
	/** The server's answer for the repository: null while unknown or found. */
	let notFound = $state<string | null>(null);
	let checking = $state(false);
	let suggestions = $state<string[]>([]);

	const errors = $derived(langStatsFormErrors(f));
	const valid = $derived(Object.keys(errors).length === 0 && notFound === null && !checking);
	const repoError = $derived(
		(touched.repository || submitted) && (repositoryError(f.repository) ?? (checking ? null : notFound))
	);
	const thresholdErr = $derived((touched.threshold || submitted) && thresholdError(f.threshold));
	const previewReady = $derived(
		repositoryError(f.repository) === null && notFound === null && !checking && thresholdError(f.threshold) === null
	);

	// Whether the repository exists, and names to suggest, as the user types (debounced).
	$effect(() => {
		const name = f.repository.trim();
		notFound = null;
		if (name === '') {
			checking = false;
			suggestions = [];
			return;
		}
		checking = true;
		let cancelled = false;
		const timer = setTimeout(async () => {
			try {
				const [exists, names] = await Promise.all([
					graphql<CheckRepositoryExistsResult, { name: string }>(CHECK_REPOSITORY_EXISTS, { name }),
					graphql<RepositoryNamesResult, { query: string; first: number }>(REPOSITORY_NAMES, { query: name, first: 10 })
				]);
				if (cancelled) return;
				notFound = exists.repository ? null : repositoryNotFound(name);
				suggestions = names.repositories.nodes.map((n) => n.name).filter((n) => n !== name);
			} catch (err) {
				if (!cancelled) notFound = errorMessage(err);
			} finally {
				if (!cancelled) checking = false;
			}
		}, 400);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
	});

	async function loadPreview(signal: AbortSignal): Promise<PreviewModel> {
		const r = await graphql<LanguageStatisticsPreviewResult, { repositories: string[] }>(
			LANGUAGE_STATISTICS_PREVIEW,
			{ repositories: [f.repository.trim()] },
			{ signal }
		);
		if (r.previewLanguageStatistics.length === 0) {
			throw new Error("We couldn't find the language statistics, try changing the repository.");
		}
		return { kind: 'pie', data: previewPieData(r.previewLanguageStatistics, f.repository, f.threshold) };
	}

	function clear(): void {
		f = defaultLangStatsFields();
		submitted = false;
		touched = {};
	}
</script>

<CreationForm
	label="Language usage trend creation form"
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
			<div class="field">
				<label for="insight-repository">Repository</label>
				<input
					id="insight-repository"
					class="input mono"
					required
					list="insight-repository-suggestions"
					autocomplete="off"
					placeholder="Example: gitlab.com/gitlab-org/gitlab"
					aria-invalid={repoError ? 'true' : undefined}
					aria-describedby="insight-repository-hint"
					aria-busy={checking}
					bind:value={f.repository}
					onblur={() => (touched = { ...touched, repository: true })}
				/>
				<datalist id="insight-repository-suggestions">
					{#each suggestions as s (s)}<option value={s}></option>{/each}
				</datalist>
				{#if repoError}
					<span class="error" id="insight-repository-hint" role="alert">{repoError}</span>
				{:else}
					<span class="hint" id="insight-repository-hint"
						>This trend is limited to one repository. You can set up multiple language usage charts for analyzing other
						repositories.</span
					>
				{/if}
			</div>
			<TitleField bind:title={f.title} {submitted} placeholder="Example: Language Usage in RepositoryName" />
			<div class="field">
				<label for="insight-threshold">Threshold of ‘Other’ category</label>
				<div class="threshold">
					<input
						id="insight-threshold"
						class="input"
						type="number"
						min="1"
						max="100"
						required
						aria-invalid={thresholdErr ? 'true' : undefined}
						aria-describedby="insight-threshold-hint"
						value={f.threshold}
						oninput={(e) => (f.threshold = e.currentTarget.value)}
						onblur={() => (touched = { ...touched, threshold: true })}
					/>
					<span aria-hidden="true">%</span>
				</div>
				{#if thresholdErr}
					<span class="error" id="insight-threshold-hint" role="alert">{thresholdErr}</span>
				{:else}
					<span class="hint" id="insight-threshold-hint"
						>Languages with usage lower than the threshold are grouped into an 'other' category.</span
					>
				{/if}
			</div>
		</section>
	{/snippet}
	{#snippet preview()}
		<LivePreview
			ready={previewReady}
			request={JSON.stringify([f.repository.trim(), f.threshold])}
			load={loadPreview}
			blankMessage="The chart preview will be shown here once you have filled out the repository field."
		/>
	{/snippet}
</CreationForm>

<style>
	.section {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}
	.threshold {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.threshold .input {
		width: 6rem;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
</style>
