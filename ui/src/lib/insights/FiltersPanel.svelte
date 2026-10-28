<script lang="ts">
	import { errorMessage } from '#lib/graphql.ts';
	import {
		EMPTY_FILTERS,
		MAX_NUMBER_OF_SAMPLES,
		MAX_NUMBER_OF_SERIES,
		clampCount,
		displayOptionsEqual,
		filtersEqual,
		hasActiveFilters,
		repoRegexError,
		repositoriesSummary,
		sortAndLimitSummary,
		type InsightFilters
	} from './insights.ts';
	import type { SeriesSortDirection, SeriesSortMode } from './queries.ts';

	interface Props {
		/** The filters applied now. */
		value: InsightFilters;
		/** The filters saved on the view. */
		original: InsightFilters;
		/** Whether the points-per-series option applies. */
		numSamples: boolean;
		/** Valid changes, applied to the card right away. */
		onchange: (filters: InsightFilters) => void;
		/** "Save/Update default filters". */
		onsave: (filters: InsightFilters) => Promise<void>;
		/** "Save as new view" with a title. */
		oncreate: (title: string) => Promise<void>;
	}

	let { value, original, numSamples, onchange, onsave, oncreate }: Props = $props();

	// svelte-ignore state_referenced_locally
	let draft = $state<InsightFilters>(structuredClone($state.snapshot(value)));
	let step = $state<'filters' | 'new-view'>('filters');
	let viewName = $state('');
	let viewNameTouched = $state(false);
	let submitting = $state(false);
	let error = $state<string | null>(null);

	const includeError = $derived(repoRegexError(draft.includeRepoRegexp));
	const excludeError = $derived(repoRegexError(draft.excludeRepoRegexp));
	const valid = $derived(!includeError && !excludeError);
	const changed = $derived(!filtersEqual(original, draft));
	const hasAppliedFilters = $derived(hasActiveFilters(original) && !changed);
	const canClear = $derived(
		hasActiveFilters(draft) || !displayOptionsEqual(original.seriesDisplayOptions, draft.seriesDisplayOptions)
	);

	function update(next: InsightFilters): void {
		draft = next;
		if (!repoRegexError(next.includeRepoRegexp) && !repoRegexError(next.excludeRepoRegexp)) {
			onchange($state.snapshot(next) as InsightFilters);
		}
	}

	function set<K extends 'context' | 'includeRepoRegexp' | 'excludeRepoRegexp'>(key: K, v: string): void {
		update({ ...draft, [key]: v });
	}

	function setSort(mode: SeriesSortMode, direction: SeriesSortDirection): void {
		update({ ...draft, seriesDisplayOptions: { ...draft.seriesDisplayOptions, sortOptions: { mode, direction } } });
	}

	function setCount(key: 'limit' | 'numSamples', input: string, max: number): void {
		update({ ...draft, seriesDisplayOptions: { ...draft.seriesDisplayOptions, [key]: clampCount(input, max) } });
	}

	function clear(): void {
		update(structuredClone(EMPTY_FILTERS));
	}

	async function save(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (!valid || !changed) return;
		submitting = true;
		error = null;
		try {
			await onsave($state.snapshot(draft) as InsightFilters);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function createView(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		viewNameTouched = true;
		if (!viewName.trim()) return;
		submitting = true;
		error = null;
		try {
			await oncreate(viewName.trim());
			step = 'filters';
			viewName = '';
			viewNameTouched = false;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	const SORTS: { label: string; options: [string, SeriesSortMode, SeriesSortDirection, string][] }[] = [
		{
			label: 'Sort by result count',
			options: [
				['Highest', 'RESULT_COUNT', 'DESC', 'Sort by result count with descending order'],
				['Lowest', 'RESULT_COUNT', 'ASC', 'Sort by result count with ascending order']
			]
		},
		{
			label: 'Sort by name',
			options: [
				['A-Z', 'LEXICOGRAPHICAL', 'ASC', 'Sort by name with ascending order'],
				['Z-A', 'LEXICOGRAPHICAL', 'DESC', 'Sort by name with descending order']
			]
		},
		{
			label: 'Sort by date added',
			options: [
				['Newest', 'DATE_ADDED', 'DESC', 'Sort by date with descending order'],
				['Oldest', 'DATE_ADDED', 'ASC', 'Sort by date with ascending order']
			]
		}
	];
</script>

{#if step === 'filters'}
	<form class="panel" aria-label="Drill-down filters panel" onsubmit={save}>
		<header>
			<h4>Filters</h4>
			<button type="button" class="btn btn-link btn-sm" disabled={!canClear} onclick={clear}>Clear filters</button>
		</header>

		<details>
			<summary>
				<span class="section">Sort &amp; Limit</span>
				<span class="preview">{sortAndLimitSummary(draft.seriesDisplayOptions)}</span>
			</summary>
			<div class="sorts">
				{#each SORTS as group (group.label)}
					<div class="sort-group">
						<small class="muted">{group.label}</small>
						<div class="toggle" role="group" aria-label={group.label}>
							{#each group.options as [label, mode, direction, aria] (label)}
								{@const selected =
									draft.seriesDisplayOptions.sortOptions.mode === mode &&
									draft.seriesDisplayOptions.sortOptions.direction === direction}
								<button
									type="button"
									class="btn btn-sm"
									class:selected
									aria-pressed={selected}
									aria-label={aria}
									onclick={() => setSort(mode, direction)}>{label}</button
								>
							{/each}
						</div>
					</div>
				{/each}
			</div>
			<label class="count">
				<span>Max number of data series to display <small class="muted">(max {MAX_NUMBER_OF_SERIES})</small></span>
				<input
					class="input"
					type="number"
					step="1"
					min="1"
					max={MAX_NUMBER_OF_SERIES}
					placeholder={String(MAX_NUMBER_OF_SERIES)}
					aria-label="Number of data series"
					value={draft.seriesDisplayOptions.limit ?? ''}
					oninput={(e) => setCount('limit', e.currentTarget.value, MAX_NUMBER_OF_SERIES)}
				/>
			</label>
			{#if numSamples}
				<label class="count">
					<span
						>Max number of series points to display <small class="muted">(max {MAX_NUMBER_OF_SAMPLES})</small></span
					>
					<input
						class="input"
						type="number"
						step="1"
						min="1"
						max={MAX_NUMBER_OF_SAMPLES}
						placeholder={String(MAX_NUMBER_OF_SAMPLES)}
						aria-label="Number of series points"
						value={draft.seriesDisplayOptions.numSamples ?? ''}
						oninput={(e) => setCount('numSamples', e.currentTarget.value, MAX_NUMBER_OF_SAMPLES)}
					/>
				</label>
			{/if}
		</details>

		<details>
			<summary>
				<span class="section">Search context</span>
				<span class="preview mono">{draft.context.trim() ? `context:${draft.context.trim()}` : ''}</span>
			</summary>
			<small class="muted">Choose a search context to change the scope of this trend.</small>
			<div class="prefixed">
				<span class="prefix mono" aria-hidden="true">context:</span>
				<input
					class="input mono"
					spellcheck="false"
					autocomplete="off"
					placeholder="global"
					aria-label="Search context"
					value={draft.context}
					oninput={(e) => set('context', e.currentTarget.value)}
				/>
			</div>
		</details>

		<details open>
			<summary>
				<span class="section">Regular expression</span>
				<span class="preview mono">{repositoriesSummary(draft)}</span>
			</summary>
			<small class="muted">Use regular expression to change the scope of this trend.</small>
			<label class="label small" for="filter-include">Include repositories</label>
			<div class="prefixed">
				<span class="prefix mono" aria-hidden="true">repo:</span>
				<input
					id="filter-include"
					class="input mono"
					spellcheck="false"
					autocomplete="off"
					placeholder="regexp-pattern"
					aria-invalid={includeError ? 'true' : undefined}
					value={draft.includeRepoRegexp}
					oninput={(e) => set('includeRepoRegexp', e.currentTarget.value)}
				/>
			</div>
			{#if includeError}<small class="error">{includeError}</small>{/if}
			<label class="label small" for="filter-exclude">Exclude repositories</label>
			<div class="prefixed">
				<span class="prefix mono" aria-hidden="true">-repo:</span>
				<input
					id="filter-exclude"
					class="input mono"
					spellcheck="false"
					autocomplete="off"
					placeholder="regexp-pattern"
					aria-invalid={excludeError ? 'true' : undefined}
					value={draft.excludeRepoRegexp}
					oninput={(e) => set('excludeRepoRegexp', e.currentTarget.value)}
				/>
			</div>
			{#if excludeError}<small class="error">{excludeError}</small>{/if}
		</details>

		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

		<footer>
			{#if hasAppliedFilters}<small class="muted">Default filters applied</small>{/if}
			<div class="buttons">
				<button type="submit" class="btn btn-sm" disabled={!valid || submitting || !changed}>
					{#if submitting}{hasAppliedFilters ? 'Updating' : 'Saving'}{:else}{hasAppliedFilters
							? 'Update default filters'
							: 'Save default filters'}{/if}
				</button>
				<button
					type="button"
					class="btn btn-sm btn-primary"
					disabled={!changed || !valid}
					onclick={() => (step = 'new-view')}>+ Save as new view</button
				>
			</div>
		</footer>
	</form>
{:else}
	<form class="panel" aria-label="Save as new view" novalidate onsubmit={createView}>
		<h4>Save as new view</h4>
		<div class="field">
			<label for="new-view-name">Name</label>
			<!-- svelte-ignore a11y_autofocus -->
			<input
				id="new-view-name"
				class="input"
				required
				autofocus
				placeholder="Example: Migration to React function components"
				bind:value={viewName}
				onblur={() => (viewNameTouched = true)}
			/>
			{#if viewNameTouched && !viewName.trim()}
				<small class="error">Trend name is a required field.</small>
			{:else}
				<small class="hint">Shown as the title for your trend</small>
			{/if}
		</div>
		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
		<div class="buttons">
			<button type="button" class="btn" onclick={() => (step = 'filters')}>Cancel</button>
			<button type="submit" class="btn btn-primary" disabled={submitting}>{submitting ? 'Saving' : 'Save'}</button>
		</div>
	</form>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		border-bottom: 1px solid var(--color-border);
		padding-bottom: var(--space-2);
	}
	h4 {
		margin: 0;
	}
	details {
		border-bottom: 1px solid var(--color-border);
		padding-bottom: var(--space-2);
	}
	details > :not(summary) {
		margin-top: var(--space-2);
	}
	summary {
		display: flex;
		gap: var(--space-2);
		align-items: baseline;
		cursor: pointer;
		list-style-position: inside;
	}
	.section {
		font-weight: 600;
		flex: none;
	}
	.preview {
		color: var(--color-text-muted);
		font-size: 0.85rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.sorts {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
	}
	.sort-group {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.toggle {
		display: flex;
	}
	.toggle .btn {
		border-radius: 0;
	}
	.toggle .btn:first-child {
		border-radius: var(--radius) 0 0 var(--radius);
	}
	.toggle .btn:last-child {
		border-radius: 0 var(--radius) var(--radius) 0;
		margin-left: -1px;
	}
	.toggle .btn.selected {
		background: var(--color-info-bg);
		color: var(--color-primary);
		border-color: var(--color-primary);
		position: relative;
	}
	.count {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		font-size: 0.9rem;
	}
	.count .input {
		width: 5rem;
		flex: none;
	}
	.prefixed {
		display: flex;
		align-items: center;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius);
		background: var(--color-surface);
	}
	.prefixed .input {
		border: 0;
		background: transparent;
	}
	.prefix {
		padding-left: var(--space-2);
		color: var(--syntax-property);
		font-size: 0.9rem;
	}
	.small {
		display: block;
		font-size: 0.9rem;
	}
	details > small {
		display: block;
	}
	.error {
		color: var(--color-danger);
	}
	footer {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.buttons {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-left: auto;
	}
	.field {
		margin-bottom: 0;
	}
</style>
