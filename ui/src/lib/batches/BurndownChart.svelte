<!--
	The burndown chart: daily counts of the published changesets by state from
	`changesetCountsOverTime`, as lines (open, draft, merged, closed) with a crosshair tooltip,
	legend, end labels and a table view. Colors are the validated categorical slots 1–4 (light on
	the white surface, dark steps on the dark surface).
-->
<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BURNDOWN_SERIES,
		CHANGESET_COUNTS_OVER_TIME,
		burndownFrom,
		compactNumber,
		niceTicks,
		type ChangesetCounts,
		type ChangesetCountsResult
	} from './batch-change.ts';

	let { batchChangeId }: { batchChangeId: string } = $props();

	const RANGES = [
		{ id: 'all', title: 'All time' },
		{ id: '90d', title: 'Last 90 days' },
		{ id: '30d', title: 'Last 30 days' },
		{ id: '7d', title: 'Last 7 days' }
	];

	let range = $state('all');
	let includeArchived = $state(false);
	let showTable = $state(false);
	let points = $state.raw<ChangesetCounts[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	$effect(() => {
		const vars = { batchChange: batchChangeId, from: burndownFrom(range), includeArchived };
		loading = true;
		error = null;
		let stale = false;
		graphql<ChangesetCountsResult, typeof vars>(CHANGESET_COUNTS_OVER_TIME, vars)
			.then((r) => {
				if (!stale) points = r.node?.changesetCountsOverTime ?? [];
			})
			.catch((err) => {
				if (!stale) error = errorMessage(err);
			})
			.finally(() => {
				if (!stale) loading = false;
			});
		return () => {
			stale = true;
		};
	});

	// Geometry.
	let width = $state(640);
	const HEIGHT = 280;
	const M = { top: 12, right: 76, bottom: 28, left: 40 };
	const plotW = $derived(Math.max(10, width - M.left - M.right));
	const plotH = HEIGHT - M.top - M.bottom;

	const maxY = $derived(Math.max(1, ...points.flatMap((p) => BURNDOWN_SERIES.map((s) => p[s.key]))));
	const ticks = $derived(niceTicks(maxY));
	const top = $derived(ticks[ticks.length - 1] ?? 1);
	const x = (i: number) => M.left + (points.length <= 1 ? plotW / 2 : (i / (points.length - 1)) * plotW);
	const y = (v: number) => M.top + plotH - (v / top) * plotH;

	const day = (iso: string) => new Date(iso).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });

	/** At most ~6 date labels along the x axis. */
	const xLabels = $derived.by(() => {
		const n = points.length;
		if (n === 0) return [];
		const step = Math.max(1, Math.ceil(n / Math.max(2, Math.floor(plotW / 90))));
		const out: number[] = [];
		for (let i = n - 1; i >= 0; i -= step) out.unshift(i);
		return out;
	});

	function path(key: (typeof BURNDOWN_SERIES)[number]['key']): string {
		return points.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(i).toFixed(1)},${y(p[key]).toFixed(1)}`).join('');
	}

	/** End labels in series order, skipped where they would collide with one already placed. */
	const endLabels = $derived.by(() => {
		const last = points[points.length - 1];
		if (!last) return [];
		const placed: { key: string; label: string; value: number; y: number }[] = [];
		for (const s of BURNDOWN_SERIES) {
			const ly = y(last[s.key]);
			if (placed.some((p) => Math.abs(p.y - ly) < 14)) continue;
			placed.push({ key: s.key, label: s.label, value: last[s.key], y: ly });
		}
		return placed;
	});

	// Crosshair: the hovered or focused point.
	let active = $state<number | null>(null);
	function onpointermove(e: PointerEvent): void {
		const svg = e.currentTarget as SVGSVGElement;
		const rect = svg.getBoundingClientRect();
		const px = ((e.clientX - rect.left) / rect.width) * width;
		if (points.length === 0) return;
		const i = points.length <= 1 ? 0 : Math.round(((px - M.left) / plotW) * (points.length - 1));
		active = Math.min(points.length - 1, Math.max(0, i));
	}
	function onkeydown(e: KeyboardEvent): void {
		if (points.length === 0) return;
		const cur = active ?? points.length - 1;
		if (e.key === 'ArrowLeft') active = Math.max(0, cur - 1);
		else if (e.key === 'ArrowRight') active = Math.min(points.length - 1, cur + 1);
		else if (e.key === 'Home') active = 0;
		else if (e.key === 'End') active = points.length - 1;
		else return;
		e.preventDefault();
	}
	const activePoint = $derived(active === null ? null : (points[active] ?? null));
	const tooltipLeft = $derived(active === null ? 0 : (x(active) / width) * 100);
</script>

<div class="burndown">
	<div class="controls">
		<div class="ranges" role="group" aria-label="Date range">
			{#each RANGES as r (r.id)}
				<button
					class="btn btn-sm"
					class:btn-primary={range === r.id}
					type="button"
					aria-pressed={range === r.id}
					onclick={() => (range = r.id)}>{r.title}</button
				>
			{/each}
		</div>
		<label class="checkbox"><input type="checkbox" bind:checked={includeArchived} /> Include archived</label>
		<span class="spacer"></span>
		<button class="btn btn-sm" type="button" aria-pressed={showTable} onclick={() => (showTable = !showTable)}>
			{showTable ? 'Show chart' : 'Show table'}
		</button>
	</div>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}

	<ul class="legend" aria-label="Legend">
		{#each BURNDOWN_SERIES as s (s.key)}
			<li><span class="key series-{s.key}" aria-hidden="true"></span>{s.label}</li>
		{/each}
	</ul>

	{#if showTable}
		<div class="table-scroll">
			<table class="table" aria-label="Changesets over time">
				<thead>
					<tr>
						<th scope="col">Date</th>
						{#each BURNDOWN_SERIES as s (s.key)}<th scope="col" class="num">{s.label}</th>{/each}
						<th scope="col" class="num">Total</th>
					</tr>
				</thead>
				<tbody>
					{#each [...points].reverse() as p (p.date)}
						<tr>
							<td>{day(p.date)}</td>
							{#each BURNDOWN_SERIES as s (s.key)}<td class="num">{p[s.key]}</td>{/each}
							<td class="num">{p.total}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else}
		<div class="plot" class:refetching={loading && points.length > 0} bind:clientWidth={width}>
			{#if points.length === 0 && !loading}
				<p class="muted empty">No published changesets yet.</p>
			{/if}
			<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
			<svg
				viewBox="0 0 {width} {HEIGHT}"
				width="100%"
				height={HEIGHT}
				role="img"
				aria-label="Burndown chart: changesets by state over time. Use the arrow keys to read values."
				tabindex="0"
				{onpointermove}
				onpointerleave={() => (active = null)}
				onblur={() => (active = null)}
				{onkeydown}
			>
				{#each ticks as t (t)}
					<line class="grid" x1={M.left} x2={M.left + plotW} y1={y(t)} y2={y(t)} />
					<text class="tick" x={M.left - 8} y={y(t)} dy="0.32em" text-anchor="end">{compactNumber(t)}</text>
				{/each}
				{#each xLabels as i (i)}
					{@const p = points[i]}
					{#if p}
						<text class="tick" x={x(i)} y={HEIGHT - 8} text-anchor="middle">{day(p.date)}</text>
					{/if}
				{/each}
				{#if points.length > 0}
					{#each BURNDOWN_SERIES as s (s.key)}
						<path class="line series-{s.key}" d={path(s.key)} data-series={s.key} />
					{/each}
					{#each BURNDOWN_SERIES as s (s.key)}
						{@const last = points[points.length - 1]}
						{#if last}
							<circle class="end series-{s.key}" cx={x(points.length - 1)} cy={y(last[s.key])} r="4" />
						{/if}
					{/each}
					{#each endLabels as l (l.key)}
						<text class="end-label" x={x(points.length - 1) + 10} y={l.y} dy="0.32em">{l.label} {l.value}</text>
					{/each}
					{#if active !== null && activePoint}
						<line class="crosshair" x1={x(active)} x2={x(active)} y1={M.top} y2={M.top + plotH} />
						{#each BURNDOWN_SERIES as s (s.key)}
							<circle class="end series-{s.key}" cx={x(active)} cy={y(activePoint[s.key])} r="4" />
						{/each}
					{/if}
				{/if}
			</svg>
			{#if activePoint}
				<div
					class="tooltip"
					role="status"
					style:left="{tooltipLeft}%"
					class:flip={tooltipLeft > 60}
				>
					<div class="tt-date">{day(activePoint.date)}</div>
					{#each BURNDOWN_SERIES as s (s.key)}
						<div class="tt-row">
							<span class="tt-key series-{s.key}" aria-hidden="true"></span>
							<strong>{activePoint[s.key]}</strong>
							<span class="muted">{s.label}</span>
						</div>
					{/each}
					<div class="tt-row"><span class="tt-key"></span><strong>{activePoint.total}</strong><span class="muted">Total</span></div>
				</div>
			{/if}
		</div>
	{/if}
	<p class="muted small">
		Published changesets only, counted from when their merge request was opened. Points are a day apart.
	</p>
</div>

<style>
	.controls {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
		margin-bottom: var(--space-3);
	}
	.ranges {
		display: flex;
		gap: var(--space-1);
		flex-wrap: wrap;
	}
	.spacer {
		flex: 1;
	}
	.legend {
		display: flex;
		gap: var(--space-4);
		list-style: none;
		padding: 0;
		margin: 0 0 var(--space-2);
		font-size: 0.875em;
		color: var(--color-text-muted);
	}
	.legend li {
		display: flex;
		align-items: center;
		gap: var(--space-1);
	}
	.key {
		display: inline-block;
		width: 14px;
		height: 2px;
		border-radius: 1px;
	}
	.plot {
		position: relative;
		transition: opacity 0.15s;
	}
	.plot.refetching {
		opacity: 0.5;
	}
	svg {
		display: block;
		overflow: visible;
	}
	svg:focus-visible {
		outline: 2px solid var(--color-focus);
		outline-offset: 2px;
	}
	.grid {
		stroke: var(--chart-grid);
		stroke-width: 1;
	}
	.tick {
		fill: var(--color-text-muted);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.line {
		fill: none;
		stroke-width: 2;
		stroke-linejoin: round;
		stroke-linecap: round;
	}
	.line.series-open {
		stroke: var(--series-open);
	}
	.line.series-draft {
		stroke: var(--series-draft);
	}
	.line.series-merged {
		stroke: var(--series-merged);
	}
	.line.series-closed {
		stroke: var(--series-closed);
	}
	.end {
		stroke: var(--color-surface);
		stroke-width: 2;
	}
	.end.series-open,
	.key.series-open,
	.tt-key.series-open {
		fill: var(--series-open);
		background: var(--series-open);
	}
	.end.series-draft,
	.key.series-draft,
	.tt-key.series-draft {
		fill: var(--series-draft);
		background: var(--series-draft);
	}
	.end.series-merged,
	.key.series-merged,
	.tt-key.series-merged {
		fill: var(--series-merged);
		background: var(--series-merged);
	}
	.end.series-closed,
	.key.series-closed,
	.tt-key.series-closed {
		fill: var(--series-closed);
		background: var(--series-closed);
	}
	.end-label {
		fill: var(--color-text);
		font-size: 11px;
	}
	.crosshair {
		stroke: var(--color-border-strong);
		stroke-width: 1;
	}
	.tooltip {
		position: absolute;
		top: 0;
		transform: translateX(12px);
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		box-shadow: var(--shadow-lg);
		padding: var(--space-2) var(--space-3);
		font-size: 0.85em;
		pointer-events: none;
		white-space: nowrap;
	}
	.tooltip.flip {
		transform: translateX(calc(-100% - 12px));
	}
	.tt-date {
		color: var(--color-text-muted);
		margin-bottom: var(--space-1);
	}
	.tt-row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.tt-row strong {
		font-variant-numeric: tabular-nums;
		min-width: 2ch;
		text-align: right;
	}
	.tt-key {
		display: inline-block;
		width: 10px;
		height: 2px;
	}
	.empty {
		position: absolute;
		inset: 40% 0 auto;
		text-align: center;
		margin: 0;
	}
	.num {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.small {
		font-size: 0.875em;
	}
</style>
