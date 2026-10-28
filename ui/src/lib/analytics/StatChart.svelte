<script lang="ts">
	import { bars, bucketLabel, formatCount, type Grouping, type StatItem } from './queries.ts';

	// One series of an `AnalyticsStatItem` as bars by day or week, with its total, a hover
	// tooltip per bar and a table view.

	interface Props {
		title: string;
		item: StatItem;
		grouping: Grouping;
		/** Which value the bars show. */
		metric?: 'count' | 'uniqueUsers';
		/** Show unique users next to the total. */
		showUsers?: boolean;
	}

	let { title, item, grouping, metric = 'count', showUsers = true }: Props = $props();

	const W = 320;
	const H = 72;
	let hovered = $state<number | null>(null);

	const values = $derived(item.nodes.map((n) => n[metric]));
	const geometry = $derived(bars(values, W, H));
	const total = $derived(metric === 'count' ? item.summary.totalCount : item.summary.totalUniqueUsers);
	const tip = $derived(hovered === null ? null : item.nodes[hovered]);
</script>

<figure class="chart">
	<figcaption>
		<span class="title">{title}</span>
		<span class="total">{formatCount(total)}</span>
		{#if showUsers && metric === 'count'}
			<span class="muted users">{formatCount(item.summary.totalUniqueUsers)} users</span>
		{/if}
	</figcaption>
	<div class="plot">
		<svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" role="img" aria-label="{title} by {grouping === 'WEEKLY' ? 'week' : 'day'}">
			<line class="baseline" x1="0" y1={H - 0.5} x2={W} y2={H - 0.5} />
			{#each geometry as b, i (i)}
				<!-- svelte-ignore a11y_no_static_element_interactions -->
				<g onmouseenter={() => (hovered = i)} onmouseleave={() => (hovered = null)}>
					<rect class="hit" x={b.x - 1} y="0" width={b.w + 2} height={H} />
					{#if b.h > 0}
						<rect class="bar" class:active={hovered === i} x={b.x} y={b.y} width={b.w} height={b.h} rx={Math.min(2, b.w / 2)} />
					{/if}
				</g>
			{/each}
		</svg>
		{#if tip}
			<div class="tip" role="status">
				<strong>{bucketLabel(tip.date, grouping)}</strong>
				<span>{formatCount(tip.count)} events</span>
				<span>{formatCount(tip.uniqueUsers)} users ({formatCount(tip.registeredUsers)} registered)</span>
			</div>
		{/if}
	</div>
	<details>
		<summary class="muted">Table</summary>
		<table class="table compact">
			<thead><tr><th scope="col">{grouping === 'WEEKLY' ? 'Week of' : 'Day'}</th><th scope="col">Events</th><th scope="col">Users</th><th scope="col">Registered</th></tr></thead>
			<tbody>
				{#each item.nodes as n (n.date)}
					<tr><td>{bucketLabel(n.date, 'DAILY')}</td><td>{n.count}</td><td>{n.uniqueUsers}</td><td>{n.registeredUsers}</td></tr>
				{/each}
			</tbody>
		</table>
	</details>
</figure>

<style>
	.chart {
		margin: 0;
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius, 6px);
		background: var(--color-surface);
		min-width: 0;
	}
	figcaption {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		flex-wrap: wrap;
		margin-bottom: var(--space-2);
	}
	.title {
		font-weight: 600;
	}
	.total {
		margin-left: auto;
		font-size: 1.3em;
		font-weight: 600;
	}
	.users {
		font-size: 0.85em;
	}
	.plot {
		position: relative;
	}
	svg {
		display: block;
		width: 100%;
		height: 72px;
	}
	.baseline {
		stroke: var(--color-border);
		stroke-width: 1;
		vector-effect: non-scaling-stroke;
	}
	.hit {
		fill: transparent;
	}
	.bar {
		fill: var(--color-primary);
		opacity: 0.85;
	}
	.bar.active {
		opacity: 1;
		fill: var(--color-primary-hover);
	}
	.tip {
		position: absolute;
		top: 0;
		right: 0;
		display: flex;
		flex-direction: column;
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius, 6px);
		background: var(--color-surface);
		color: var(--color-text);
		font-size: 0.8em;
		pointer-events: none;
	}
	details {
		margin-top: var(--space-2);
		font-size: 0.85em;
	}
	summary {
		cursor: pointer;
	}
	.compact :global(td),
	.compact :global(th) {
		padding: 2px var(--space-2);
	}
</style>
