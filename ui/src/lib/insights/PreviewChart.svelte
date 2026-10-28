<!--
	The chart of a creation page's live preview: the dashboard cards' line, bar and pie
	charts drawn from preview data, with a legend for line charts.
-->
<script lang="ts">
	import BarChart from './BarChart.svelte';
	import LineChart from './LineChart.svelte';
	import PieChart from './PieChart.svelte';
	import type { PreviewModel } from './creation.ts';

	interface Props {
		model: PreviewModel;
	}

	let { model }: Props = $props();

	let width = $state(0);
	let height = $state(0);
</script>

<div class="preview-chart" data-chart={model.kind}>
	<div class="chart" bind:clientWidth={width} bind:clientHeight={height}>
		{#if width > 0 && height > 0}
			<div class="canvas">
				{#if model.kind === 'line'}
					<LineChart series={model.series} {width} {height} />
				{:else if model.kind === 'bar'}
					<BarChart data={model.data} {width} {height} />
				{:else}
					<PieChart data={model.data} {width} {height} />
				{/if}
			</div>
		{/if}
	</div>
	{#if model.kind === 'line' && model.series.length > 0}
		<ul class="legend" aria-label="Legend">
			{#each model.series as s (s.id)}
				<li><span class="swatch" style:background={s.color}></span><span class="name">{s.name}</span></li>
			{/each}
		</ul>
	{/if}
</div>

<style>
	.preview-chart {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		height: 100%;
	}
	.chart {
		position: relative;
		flex: 1;
		min-height: 14rem;
	}
	.canvas {
		position: absolute;
		inset: 0;
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
	.legend li {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		min-width: 0;
	}
	.swatch {
		width: 0.65rem;
		height: 0.65rem;
		border-radius: 50%;
		flex: none;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
