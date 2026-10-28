<!-- A badge for a changeset, bulk change or bulk operation state, with the label in text. -->
<script lang="ts">
	import { enumLabel } from './changesets.ts';

	interface Props {
		state: string;
		/** success, merged, danger, info, warning or muted. */
		tone?: string;
		label?: string;
	}

	let { state, tone = 'muted', label }: Props = $props();
</script>

<span class="state-badge tone-{tone}" data-state={state}>{label ?? enumLabel(state)}</span>

<style>
	.state-badge {
		display: inline-block;
		padding: 0.05rem 0.5rem;
		border-radius: 999px;
		font-size: 0.8em;
		font-weight: 600;
		white-space: nowrap;
		border: 1px solid transparent;
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}
	.tone-success {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.tone-danger {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.tone-warning {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
	.tone-info {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.tone-merged {
		background: color-mix(in srgb, var(--series-merged) 14%, transparent);
		color: var(--series-merged);
	}
</style>
