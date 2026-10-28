<!--
	The language usage pie: one slice per language over the trend's threshold, the
	rest as "Other", each labelled with its share and linked to a search of that language in
	the trend's repositories. The hovered slice is drawn on top, the others faded.
-->
<script lang="ts">
	import { PIE_LABEL_HEIGHT, pieArcs, pieLabels, type PieDatum } from './charts.ts';

	interface Props {
		data: PieDatum[];
		width: number;
		height: number;
		/** Called when a slice is clicked to open its search. */
		onpointclick?: () => void;
	}

	let { data, width, height, onpointclick }: Props = $props();

	const PAD = 20;
	/** Room for a label beside the pie: the leader line and about 10 characters. */
	const LABEL = 90;
	let hovered = $state<string | null>(null);

	const radius = $derived(
		Math.max(Math.min((width - 2 * LABEL) / 2.4, (height - 2 * PAD) / 2.6), 10)
	);
	const arcs = $derived(pieArcs(data, radius));
	const labels = $derived(
		pieLabels(
			arcs.map((a) => a.mid),
			radius
		)
	);
	const hoveredArc = $derived(arcs.find((a) => a.datum.name === hovered) ?? null);

	function percent(share: number): string {
		return `${(share * 100).toFixed(2)}%`;
	}
</script>

<svg {width} {height} role="group" aria-label="Pie chart">
	<g transform={`translate(${width / 2},${height / 2})`}>
		{#each arcs as arc, i (arc.datum.name)}
			{@const label = labels[i]}
			<a
				href={arc.datum.link ?? undefined}
				target="_blank"
				rel="noopener"
				class="slice"
				class:faded={hovered !== null && hovered !== arc.datum.name}
				aria-label={`Name: ${arc.datum.name}. Value: ${percent(arc.datum.share)}.`}
				data-language={arc.datum.name}
				onpointerenter={() => (hovered = arc.datum.name)}
				onpointerleave={() => (hovered = null)}
				onfocus={() => (hovered = arc.datum.name)}
				onblur={() => (hovered = null)}
				onclick={() => {
					if (arc.datum.link) onpointclick?.();
				}}
			>
				<path class="arc" d={arc.path} fill={arc.datum.color} />
				{#if label}
					<path class="leader" d={label.line} />
					<text x={label.x} y={label.y - PIE_LABEL_HEIGHT / 4} text-anchor={label.anchor} dominant-baseline="middle">
						<tspan class="name">{arc.datum.name}</tspan>
						<tspan class="share" x={label.x} dy="1.15em">{percent(arc.datum.share)}</tspan>
					</text>
				{/if}
			</a>
		{/each}
		{#if hoveredArc}
			<path class="arc top" d={hoveredArc.path} fill={hoveredArc.datum.color} aria-hidden="true" pointer-events="none" />
		{/if}
	</g>
</svg>

<style>
	svg {
		display: block;
		overflow: visible;
	}
	.slice {
		transition: opacity 120ms;
	}
	.slice.faded {
		opacity: 0.45;
	}
	.slice:focus-visible {
		outline: none;
	}
	.slice:focus-visible .arc,
	.arc.top {
		stroke: var(--color-text);
		stroke-width: 2;
	}
	.arc {
		stroke: var(--color-surface);
		stroke-width: 2;
		stroke-linejoin: round;
	}
	.leader {
		fill: none;
		stroke: var(--color-border-strong);
	}
	text {
		font-size: 12px;
	}
	.name {
		fill: var(--color-text);
		font-weight: 600;
	}
	.share {
		fill: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
	}
</style>
