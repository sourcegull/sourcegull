<!--
	The bar chart of a group-by trend: one bar per group (repository, language, path,
	author or date) with its total, a tooltip on hover. Bars have no drill-down.
-->
<script lang="ts">
	import { formatValue, niceTicks, type BarDatum } from './charts.ts';

	interface Props {
		data: BarDatum[];
		width: number;
		height: number;
	}

	let { data, width, height }: Props = $props();

	const TOP = 12;
	const RIGHT = 8;
	const BOTTOM = 28;

	let active = $state<number | null>(null);
	const activeDatum = $derived(active === null ? undefined : data[active]);

	const y = $derived(
		niceTicks(
			0,
			Math.max(0, ...data.map((d) => d.value)),
			Math.max(2, Math.floor(height / 50))
		)
	);
	const yLabels = $derived(y.ticks.map(formatValue));
	const left = $derived(12 + Math.max(...yLabels.map((l) => l.length), 1) * 7);
	const innerW = $derived(Math.max(width - left - RIGHT, 10));
	const innerH = $derived(Math.max(height - TOP - BOTTOM, 10));
	const band = $derived(innerW / Math.max(data.length, 1));
	const barW = $derived(Math.max(Math.min(band - 2, 48), 1));
	/** Category labels fit when a band is wide enough for a few characters. */
	const showLabels = $derived(band >= 28);
	const labelChars = $derived(Math.max(Math.floor(band / 7), 3));

	function py(v: number): number {
		return innerH - (v / (y.max || 1)) * innerH;
	}
	function short(s: string): string {
		return s.length > labelChars ? `${s.slice(0, labelChars - 1)}…` : s;
	}
	function barPath(x: number, w: number, top: number): string {
		const r = Math.min(4, w / 2, innerH - top);
		return `M${x},${innerH}V${top + r}Q${x},${top} ${x + r},${top}H${x + w - r}Q${x + w},${top} ${x + w},${top + r}V${innerH}Z`;
	}
</script>

<div class="bar-chart" style:height={`${height}px`}>
	<svg {width} {height} role="group" aria-label="Bar chart">
		<g transform={`translate(${left},${TOP})`}>
			<g aria-hidden="true">
				{#each y.ticks as t, i (t)}
					<line class="grid" x1="0" x2={innerW} y1={py(t)} y2={py(t)} />
					<text class="tick" x="-8" y={py(t)} text-anchor="end" dominant-baseline="middle">{yLabels[i]}</text>
				{/each}
			</g>
			<g role="list">
				{#each data as d, i (d.name)}
					{@const x = i * band + (band - barW) / 2}
					<g
						role="listitem"
						class="bar"
						class:faded={active !== null && active !== i}
						aria-label={`${d.name}: ${d.value}`}
						onpointerenter={() => (active = i)}
						onpointerleave={() => (active = null)}
					>
						<rect class="hit" x={i * band} y="0" width={band} height={innerH} />
						{#if d.value > 0}<path d={barPath(x, barW, py(d.value))} fill={d.color} />{/if}
						{#if showLabels}
							<text class="tick" x={x + barW / 2} y={innerH + 16} text-anchor="middle">{short(d.name)}</text>
						{/if}
					</g>
				{/each}
			</g>
			<line class="axis" x1="0" x2={innerW} y1={innerH} y2={innerH} aria-hidden="true" />
		</g>
	</svg>
	{#if activeDatum && active !== null}
		{@const d = activeDatum}
		{@const cx = left + active * band + band / 2}
		<div
			class="tooltip card"
			role="tooltip"
			style:left={cx > width / 2 ? 'auto' : `${cx + 8}px`}
			style:right={cx > width / 2 ? `${width - cx + 8}px` : 'auto'}
		>
			<span class="swatch" style:background={d.color}></span>
			<span class="name">{d.name}</span>
			<span class="value">{formatValue(d.value)}</span>
		</div>
	{/if}
</div>

<style>
	.bar-chart {
		position: relative;
	}
	svg {
		display: block;
	}
	.grid {
		stroke: var(--chart-grid);
	}
	.axis {
		stroke: var(--color-border-strong);
	}
	.tick {
		fill: var(--color-text-muted);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.hit {
		fill: transparent;
	}
	.bar {
		transition: opacity 120ms;
	}
	.bar.faded {
		opacity: 0.45;
	}
	.tooltip {
		position: absolute;
		top: 0;
		z-index: 4;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		max-width: min(18rem, 80%);
		padding: var(--space-2) var(--space-3);
		font-size: 0.8rem;
		pointer-events: none;
		box-shadow: var(--shadow-lg);
	}
	.swatch {
		width: 0.6rem;
		height: 0.6rem;
		border-radius: 50%;
		flex: none;
	}
	.name {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.value {
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
</style>
