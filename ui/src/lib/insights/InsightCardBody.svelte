<!--
	The chart of a trend card: a line chart with its legend (search and capture-group
	trends), a bar chart (group-by trends) or a language pie, with overlays
	("This trend is still being processed", "No data to display"). The legend of several
	series is interactive: clicking a series shows only the selected ones, hovering fades the rest.
-->
<script lang="ts">
	import { EVENTS, logEvent } from '#lib/events.ts';
	import BarChart from './BarChart.svelte';
	import LineChart from './LineChart.svelte';
	import PieChart from './PieChart.svelte';
	import {
		barData,
		chartKind,
		hasNoData,
		incompleteAlertMessage,
		lineSeries,
		pieData,
		toggleSeries,
		tooltipDate,
		visibleSeries,
		type LineSeries
	} from './charts.ts';
	import { isLoadingData } from './insights.ts';
	import type { InsightViewNode } from './queries.ts';

	interface Props {
		view: InsightViewNode;
		/** "Start Y Axis at 0" (line charts). */
		zeroYAxisMin?: boolean;
	}

	let { view, zeroYAxisMin = false }: Props = $props();

	/** Below this width the legend goes under the chart. */
	const MIN_ASIDE_WIDTH = 460;
	/** The legend goes aside only with more series than this. */
	const MIN_ASIDE_SERIES = 3;

	let width = $state(0);
	let chartWidth = $state(0);
	let chartHeight = $state(0);
	let selected = $state<string[]>([]);
	let hoveredId = $state<string | null>(null);

	const kind = $derived(chartKind(view));
	const series = $derived(kind === 'line' ? lineSeries(view) : []);
	const shown = $derived(visibleSeries(series, selected));
	const bars = $derived(kind === 'bar' ? barData(view) : []);
	const slices = $derived(kind === 'pie' ? pieData(view) : []);
	const processing = $derived(kind !== 'pie' && isLoadingData(view));
	const empty = $derived(hasNoData(view));
	const aside = $derived(kind === 'line' && width >= MIN_ASIDE_WIDTH && series.length > MIN_ASIDE_SERIES);

	// A new response resets the legend selection.
	$effect.pre(() => {
		void view;
		selected = [];
	});

	const pointClicked = (): void => logEvent(EVENTS.insightDataPointClick, { type: kind });

	function alertTitle(s: LineSeries): string {
		const dates = s.alerts.map((a) => tooltipDate(Date.parse(a.time))).join(', ');
		return s.alerts[0] ? `${incompleteAlertMessage(s.alerts[0])} Points: ${dates}` : '';
	}
</script>

<div class="body" class:aside data-testid="insight-card-body" data-chart={kind} bind:clientWidth={width}>
	<div class="chart" bind:clientWidth={chartWidth} bind:clientHeight={chartHeight}>
		{#if chartWidth > 0 && chartHeight > 0}
			<div class="canvas" role="presentation" onpointerenter={() => logEvent(EVENTS.insightHover, { type: kind })}>
				{#if kind === 'line'}
					<LineChart series={shown} width={chartWidth} height={chartHeight} {zeroYAxisMin} {hoveredId} onpointclick={pointClicked} />
				{:else if kind === 'bar'}
					<BarChart data={bars} width={chartWidth} height={chartHeight} />
				{:else if slices.length > 0}
					<PieChart data={slices} width={chartWidth} height={chartHeight} onpointclick={pointClicked} />
				{/if}
			</div>
		{/if}
		{#if processing}
			<div class="overlay" role="status">
				<svg viewBox="0 0 24 24" width="32" height="32" aria-hidden="true"
					><path
						fill="currentColor"
						d="M12 4V2A10 10 0 0 0 2 12h2a8 8 0 0 1 8-8Zm0 16a8 8 0 0 1-8-8H2a10 10 0 0 0 10 10v-2Zm8-8a8 8 0 0 1-8 8v2a10 10 0 0 0 10-10h-2Zm-8-8v2a6 6 0 0 1 6 6h2a8 8 0 0 0-8-8Z"
					/></svg
				>
				<h4>This trend is still being processed</h4>
			</div>
		{:else if empty}
			<div class="overlay" role="status">
				<h4>No data to display</h4>
				<small class="muted">We couldn’t find any matches for this trend.</small>
			</div>
		{/if}
	</div>

	{#if kind === 'line' && series.length > 0}
		<ul class="legend" aria-label="Legend">
			{#each series as s (s.id)}
				{@const isSelected = selected.includes(s.id)}
				<li class:active={isSelected || hoveredId === s.id}>
					{#if series.length > 1}
						<button
							type="button"
							role="checkbox"
							aria-checked={isSelected}
							onpointerenter={() => (hoveredId = s.id)}
							onpointerleave={() => (hoveredId = null)}
							onfocus={() => (hoveredId = s.id)}
							onblur={() => (hoveredId = null)}
							onclick={() =>
								(selected = toggleSeries(
									selected,
									s.id,
									series.map((x) => x.id)
								))}
						>
							<span class="swatch" style:background={s.color}></span>
							<span class="name">{s.name}</span>
						</button>
					{:else}
						<span class="item"><span class="swatch" style:background={s.color}></span><span class="name">{s.name}</span></span>
					{/if}
					{#if s.alerts.length > 0}
						<span class="incomplete" role="img" aria-label="Trend is in incomplete state" title={alertTitle(s)}>!</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
	{#if kind === 'line' && view.seriesCount !== null && view.seriesCount > view.dataSeries.length}
		<p class="muted more">Showing {view.dataSeries.length} of {view.seriesCount} series</p>
	{/if}
</div>

<style>
	.body {
		flex: 1;
		min-height: 0;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.body.aside {
		flex-direction: row;
	}
	.chart {
		position: relative;
		flex: 1;
		min-width: 0;
		min-height: 12rem;
	}
	.canvas {
		position: absolute;
		inset: 0;
	}
	.overlay {
		position: absolute;
		inset: 0;
		z-index: 3;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: var(--space-1);
		padding: var(--space-3);
		text-align: center;
		background: color-mix(in srgb, var(--color-surface) 80%, transparent);
		color: var(--color-text-muted);
	}
	.overlay h4 {
		margin: 0;
		color: var(--color-text);
		font-size: 0.95rem;
	}
	.legend {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1) var(--space-3);
		max-height: 6rem;
		overflow-y: auto;
		font-size: 0.85rem;
	}
	.aside .legend {
		flex: 0 0 30%;
		flex-direction: column;
		flex-wrap: nowrap;
		max-height: none;
	}
	.legend li {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		min-width: 0;
		max-width: 100%;
	}
	.legend button,
	.legend .item {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		min-width: 0;
		padding: 1px var(--space-1);
		border: 1px solid transparent;
		border-radius: 4px;
		background: none;
		color: var(--color-text);
		font: inherit;
		text-align: left;
	}
	.legend button {
		cursor: pointer;
	}
	.legend li.active button {
		border-color: var(--color-border-strong);
		background: var(--color-bg-muted);
	}
	.swatch {
		width: 0.65rem;
		height: 0.65rem;
		border-radius: 50%;
		flex: none;
	}
	.name {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.incomplete {
		display: inline-grid;
		place-items: center;
		width: 1rem;
		height: 1rem;
		border-radius: 50%;
		background: var(--color-warning-bg);
		color: var(--color-warning);
		font-size: 0.7rem;
		font-weight: 700;
		cursor: help;
		flex: none;
	}
	.more {
		margin: 0;
		font-size: 0.85rem;
	}
</style>
