<script lang="ts">
	import { stepRows, type IndexSteps } from './codegraph.ts';

	interface Props {
		steps: IndexSteps;
	}

	let { steps }: Props = $props();

	const rows = $derived(stepRows(steps));

	function duration(ms: number | null): string {
		if (ms === null) return '';
		return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`;
	}
</script>

<ol class="steps" aria-label="Index job steps">
	{#each rows as r, i (i)}
		<li class="step" data-step={r.entry?.key ?? r.label}>
			<div class="head">
				<span class="label">{r.label}</span>
				{#if r.entry === null}
					<span class="badge state-muted">not run</span>
				{:else if r.entry.exitCode === null}
					<span class="badge state-info">running</span>
				{:else if r.entry.exitCode === 0}
					<span class="badge state-success">exit 0</span>
				{:else}
					<span class="badge state-danger">exit {r.entry.exitCode}</span>
				{/if}
				{#if r.entry}<span class="muted">{duration(r.entry.durationMilliseconds)}</span>{/if}
			</div>
			{#if r.detail}<code class="detail">{r.detail}</code>{/if}
			{#if r.entry?.out}<pre class="out">{r.entry.out}</pre>{/if}
		</li>
	{/each}
</ol>

<style>
	.steps {
		list-style: none;
		margin: var(--space-2) 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.head {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.label {
		font-weight: 600;
	}
	.detail {
		display: block;
		font-size: 0.85em;
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}
	.out {
		margin: var(--space-1) 0 0;
		padding: var(--space-2);
		max-height: 20rem;
		overflow: auto;
		background: var(--color-bg-subtle, var(--color-info-bg));
		border-radius: var(--radius);
		font-size: 0.8em;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.state-success {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.state-danger {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.state-info {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.state-muted {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
</style>
