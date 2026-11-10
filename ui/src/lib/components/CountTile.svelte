<script lang="ts">
	/** A large count with a label, which filters a list when clicked. */
	let {
		value,
		label,
		active = false,
		tone,
		title,
		disabled = false,
		onclick
	}: {
		/** Null while loading. */
		value: number | null;
		label: string;
		/** Whether the list is filtered by this tile. */
		active?: boolean;
		/** Colours the number: `primary` for a notable count, `warning` for one to act on. */
		tone?: 'primary' | 'warning';
		title?: string;
		disabled?: boolean;
		onclick: () => void;
	} = $props();
</script>

<button type="button" class="tile" class:active aria-pressed={active} {title} {disabled} {onclick}>
	<span class="value {tone ?? ''}">{value === null ? '–' : value.toLocaleString('en-US')}</span><span class="label"
		>{label}</span
	>
</button>

<style>
	.tile {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-surface);
		color: var(--color-text);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.tile:hover:not(:disabled) {
		background: var(--color-bg-hover);
		border-color: var(--color-border-strong);
	}
	.tile.active {
		border-color: var(--color-primary);
		box-shadow: inset 0 0 0 1px var(--color-primary);
	}
	.tile:disabled {
		cursor: default;
	}
	.value {
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.value.primary {
		color: var(--color-primary);
	}
	.value.warning {
		color: var(--color-warning);
	}
	.label {
		color: var(--color-text-muted);
	}
</style>
