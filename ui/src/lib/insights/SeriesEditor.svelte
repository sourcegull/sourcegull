<!--
	"Data series" of the search and group results creation pages: each series has a name,
	a color and a search query (checked: no `context:`, `repo:` or `rev:`
	filters); series can be added and removed.
-->
<script lang="ts">
	import { DEFAULT_SERIES_COLORS } from './charts.ts';
	import { newSeries, seriesErrors, seriesListError, type EditableSeries } from './creation.ts';

	interface Props {
		series: EditableSeries[];
		submitted: boolean;
		/** Group results trends draw every series in the first one's color. */
		colors?: boolean;
		/** The most series (none: any number). */
		max?: number;
	}

	let { series = $bindable(), submitted, colors = true, max }: Props = $props();

	let touched = $state<Record<string, { name?: boolean; query?: boolean }>>({});

	const listError = $derived(series.length === 0 || submitted ? seriesListError(series) : null);

	function update(key: string, patch: Partial<EditableSeries>): void {
		series = series.map((s) => (s.key === key ? { ...s, ...patch } : s));
	}

	function touch(key: string, field: 'name' | 'query'): void {
		touched = { ...touched, [key]: { ...touched[key], [field]: true } };
	}

	function addSeries(): void {
		series = [...series, newSeries(series.length)];
	}
</script>

<fieldset class="section">
	<legend><h2>Data series</h2></legend>
	<p class="muted subtitle">Add any number of data series to your chart</p>
	<ol class="series">
		{#each series as s, i (s.key)}
			{@const errors = seriesErrors(s)}
			{@const t = touched[s.key] ?? {}}
			{@const nameError = (t.name || submitted) && errors.name}
			{@const queryError = (t.query || submitted) && errors.query}
			<li class="card series-card" aria-label={`Data series ${i + 1}`}>
				<div class="field">
					<label for={`series-name-${s.key}`}>Name</label>
					<input
						id={`series-name-${s.key}`}
						class="input"
						placeholder="Example: Function component"
						value={s.name}
						aria-invalid={nameError ? 'true' : undefined}
						oninput={(e) => update(s.key, { name: e.currentTarget.value })}
						onblur={() => touch(s.key, 'name')}
					/>
					{#if nameError}<span class="error" role="alert">{nameError}</span>{/if}
				</div>
				<div class="field">
					<label for={`series-query-${s.key}`}>Search query</label>
					<input
						id={`series-query-${s.key}`}
						class="input mono"
						placeholder="Example: patternType:regexp const\s\w+:\s(React\.)?FunctionComponent"
						value={s.query}
						aria-invalid={queryError ? 'true' : undefined}
						oninput={(e) => update(s.key, { query: e.currentTarget.value })}
						onblur={() => touch(s.key, 'query')}
					/>
					{#if queryError}
						<span class="error" role="alert">{queryError}</span>
					{:else}
						<span class="hint muted">Do not include the <code>repo:</code> filter; it will be added automatically, if needed.</span>
					{/if}
				</div>
				{#if colors}
					<div class="field">
						<span class="label" id={`series-color-${s.key}`}>Color</span>
						<div class="colors" role="radiogroup" aria-labelledby={`series-color-${s.key}`}>
							{#each DEFAULT_SERIES_COLORS as c (c)}
								<button
									type="button"
									role="radio"
									class="color"
									aria-checked={s.stroke === c}
									aria-label={c}
									style:background={c}
									onclick={() => update(s.key, { stroke: c })}
								></button>
							{/each}
							<input
								type="color"
								class="custom"
								aria-label="Custom color"
								value={s.stroke.startsWith('#') && s.stroke.length === 7 ? s.stroke : '#000000'}
								oninput={(e) => update(s.key, { stroke: e.currentTarget.value })}
							/>
						</div>
					</div>
				{/if}
				{#if series.length > 1}
					<div class="series-actions">
						<button
							type="button"
							class="btn btn-sm btn-danger"
							onclick={() => (series = series.filter((x) => x.key !== s.key))}>Remove series</button
						>
					</div>
				{/if}
			</li>
		{/each}
	</ol>
	{#if max === undefined || series.length < max}
		<button type="button" class="btn add" onclick={addSeries}>+ Add another data series</button>
	{/if}
	{#if listError}<span class="error" role="alert">{listError}</span>{/if}
</fieldset>

<style>
	.section {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
	}
	legend {
		padding: 0;
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
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	.series-card {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	.colors {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-1);
	}
	.color {
		width: 1.5rem;
		height: 1.5rem;
		border-radius: 50%;
		border: 2px solid transparent;
		cursor: pointer;
		padding: 0;
	}
	.color[aria-checked='true'] {
		outline: 2px solid var(--color-text);
		outline-offset: 1px;
	}
	.custom {
		width: 2rem;
		height: 1.6rem;
		padding: 0;
		border: 1px solid var(--color-border);
		background: none;
	}
	.series-actions {
		display: flex;
		justify-content: flex-end;
	}
	.add {
		align-self: flex-start;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
	.hint {
		font-size: 0.85rem;
	}
</style>
