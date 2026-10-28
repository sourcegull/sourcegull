<!--
	A time series line chart (search and capture-group trends): one line per series,
	a tooltip listing every series at the hovered date, and points that link to their
	drill-down search (a `type:diff` query between the previous point and this one).
	Clicking anywhere in the plot opens the active point's search.
-->
<script lang="ts">
	import {
		dateTickFormatter,
		formatValue,
		linePath,
		nearestIndex,
		nearestSeries,
		thinTicks,
		tooltipDate,
		tooltipLines,
		xValues,
		yDomain,
		type LineSeries
	} from './charts.ts';

	interface Props {
		series: LineSeries[];
		width: number;
		height: number;
		zeroYAxisMin?: boolean;
		/** The series hovered in the legend: others are faded. */
		hoveredId?: string | null;
		/** Called when a point is clicked to open its search. */
		onpointclick?: () => void;
	}

	let { series, width, height, zeroYAxisMin = false, hoveredId = null, onpointclick }: Props = $props();

	const TOP = 12;
	const RIGHT = 16;
	const BOTTOM = 26;

	let active = $state<{ x: number; id: string | null } | null>(null);

	const xs = $derived(xValues(series));
	const y = $derived(yDomain(series, zeroYAxisMin, Math.max(2, Math.floor(height / 50))));
	const yLabels = $derived(y.ticks.map(formatValue));
	const left = $derived(12 + Math.max(...yLabels.map((l) => l.length), 1) * 7);
	const innerW = $derived(Math.max(width - left - RIGHT, 10));
	const innerH = $derived(Math.max(height - TOP - BOTTOM, 10));
	const xMin = $derived(xs[0] ?? 0);
	const xMax = $derived(xs.at(-1) ?? 1);
	const formatDate = $derived(dateTickFormatter(xs));
	const xTicks = $derived(thinTicks(xs, Math.max(2, Math.floor(innerW / 70))));

	function px(x: number): number {
		return xMax === xMin ? innerW / 2 : ((x - xMin) / (xMax - xMin)) * innerW;
	}
	function py(v: number): number {
		return innerH - ((v - y.min) / (y.max - y.min || 1)) * innerH;
	}

	const lines = $derived(
		series.map((s) => ({
			...s,
			path: linePath(s.points.map((p) => ({ px: px(p.x), py: py(p.y) })))
		}))
	);

	const tooltip = $derived(active ? tooltipLines(series, active.x, active.id) : null);
	const activePoint = $derived.by(() => {
		if (!active) return null;
		const s = series.find((l) => l.id === active!.id);
		return s?.points.find((p) => p.x === active!.x) ?? null;
	});

	function onpointermove(e: PointerEvent): void {
		const rect = (e.currentTarget as SVGRectElement).getBoundingClientRect();
		const fx = (e.clientX - rect.left) / (rect.width || 1);
		const fy = (e.clientY - rect.top) / (rect.height || 1);
		const i = nearestIndex(xs, xMin + fx * (xMax - xMin));
		const x = xs[i];
		if (x === undefined) return;
		active = { x, id: nearestSeries(series, x, y.max - fy * (y.max - y.min)) };
	}

	function onplotclick(): void {
		if (!activePoint?.link) return;
		onpointclick?.();
		window.open(activePoint.link, '_blank', 'noopener');
	}

	const tooltipLeft = $derived(active ? left + px(active.x) : 0);
	const tooltipOnLeft = $derived(tooltipLeft > width / 2);
</script>

<div class="line-chart" style:height={`${height}px`}>
	<svg {width} {height} role="group" aria-label="Line chart">
		<g transform={`translate(${left},${TOP})`}>
			<g class="grid" aria-hidden="true">
				{#each y.ticks as t, i (t)}
					<line x1="0" x2={innerW} y1={py(t)} y2={py(t)} />
					<text class="tick" x="-8" y={py(t)} text-anchor="end" dominant-baseline="middle">{yLabels[i]}</text>
				{/each}
				<line class="axis" x1="0" x2={innerW} y1={innerH} y2={innerH} />
				{#each xTicks as t (t)}
					<line class="axis" x1={px(t)} x2={px(t)} y1={innerH} y2={innerH + 4} />
					<text class="tick" x={px(t)} y={innerH + 17} text-anchor="middle">{formatDate(t)}</text>
				{/each}
			</g>
			{#if active}
				<line class="crosshair" aria-hidden="true" x1={px(active.x)} x2={px(active.x)} y1="0" y2={innerH} />
			{/if}
			<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
			<rect
				class="plot-area"
				class:clickable={activePoint?.link}
				x="0"
				y="0"
				width={innerW}
				height={innerH}
				{onpointermove}
				onpointerleave={() => (active = null)}
				onclick={onplotclick}
			/>
			{#each lines as s (s.id)}
				<g
					class="series"
					class:faded={hoveredId !== null && hoveredId !== s.id}
					role="group"
					aria-label={s.name}
					data-series={s.id}
				>
					<path class="line" d={s.path} stroke={s.color} />
					{#each s.points as p (p.x)}
						{@const isActive = active?.x === p.x && active?.id === s.id}
						<a
							href={p.link ?? undefined}
							target="_blank"
							rel="noopener"
							class="point"
							aria-label={`${s.name}: ${p.y} on ${tooltipDate(p.x)}`}
							onpointerenter={() => (active = { x: p.x, id: s.id })}
							onfocus={() => (active = { x: p.x, id: s.id })}
							onblur={() => (active = null)}
							onclick={() => {
								if (p.link) onpointclick?.();
							}}
						>
							<circle class="hit" cx={px(p.x)} cy={py(p.y)} r="8" />
							<circle class="dot" class:active={isActive} cx={px(p.x)} cy={py(p.y)} r={isActive ? 5 : 3.5} fill={s.color} />
						</a>
					{/each}
				</g>
			{/each}
		</g>
	</svg>
	{#if active && tooltip && tooltip.items.length > 0}
		<div
			class="tooltip card"
			role="tooltip"
			style:left={tooltipOnLeft ? 'auto' : `${tooltipLeft + 12}px`}
			style:right={tooltipOnLeft ? `${width - tooltipLeft + 12}px` : 'auto'}
		>
			<div class="date">{tooltipDate(active.x)}</div>
			<ul>
				{#if tooltip.before > 0}<li class="more">… and {tooltip.before} more</li>{/if}
				{#each tooltip.items as l (l.id)}
					<li class:active={l.id === active.id}>
						<span class="swatch" style:background={l.color}></span>
						<span class="name">{l.name}</span>
						<span class="value">{formatValue(l.value)}</span>
					</li>
				{/each}
				{#if tooltip.after > 0}<li class="more">… and {tooltip.after} more</li>{/if}
			</ul>
			{#if activePoint?.link}<div class="hint">Click to see the changes in this period</div>{/if}
		</div>
	{/if}
</div>

<style>
	.line-chart {
		position: relative;
	}
	svg {
		display: block;
		overflow: visible;
	}
	.grid line {
		stroke: var(--chart-grid);
		stroke-width: 1;
	}
	.grid line.axis {
		stroke: var(--color-border-strong);
	}
	.tick {
		fill: var(--color-text-muted);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.crosshair {
		stroke: var(--color-border-strong);
		stroke-dasharray: 3 3;
		pointer-events: none;
	}
	.plot-area {
		fill: transparent;
	}
	.plot-area.clickable {
		cursor: pointer;
	}
	.series {
		transition: opacity 120ms;
	}
	.series.faded {
		opacity: 0.2;
	}
	.line {
		fill: none;
		stroke-width: 2;
		stroke-linejoin: round;
		stroke-linecap: round;
		pointer-events: none;
	}
	.point .hit {
		fill: transparent;
	}
	.point .dot {
		stroke: var(--color-surface);
		stroke-width: 1.5;
	}
	.point:focus-visible {
		outline: none;
	}
	.point:focus-visible .dot,
	.point .dot.active {
		stroke: var(--color-text);
	}
	.tooltip {
		position: absolute;
		top: 0;
		z-index: 4;
		min-width: 10rem;
		max-width: min(18rem, 80%);
		padding: var(--space-2) var(--space-3);
		font-size: 0.8rem;
		pointer-events: none;
		box-shadow: var(--shadow-lg);
	}
	.date {
		font-weight: 600;
		margin-bottom: var(--space-1);
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	li {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 1px 0;
	}
	li.active {
		font-weight: 600;
	}
	li.more {
		color: var(--color-text-muted);
	}
	.swatch {
		width: 0.6rem;
		height: 0.6rem;
		border-radius: 50%;
		flex: none;
	}
	.name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.value {
		font-variant-numeric: tabular-nums;
	}
	.hint {
		margin-top: var(--space-1);
		color: var(--color-text-muted);
		font-size: 0.75rem;
	}
</style>
