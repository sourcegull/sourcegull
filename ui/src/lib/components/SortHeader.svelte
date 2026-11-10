<script lang="ts" generics="T extends string">
	/** A table header that sorts by `key` when clicked, and shows the direction when it does. */
	let {
		key,
		label,
		order,
		descending,
		onsort
	}: {
		key: T;
		label: string;
		/** The current sort key. */
		order: T;
		descending: boolean;
		onsort: (key: T) => void;
	} = $props();

	const sorted = $derived(order === key);
</script>

<th scope="col" aria-sort={sorted ? (descending ? 'descending' : 'ascending') : 'none'}
	><button type="button" onclick={() => onsort(key)}>{label}{sorted ? (descending ? ' ↓' : ' ↑') : ''}</button></th
>

<style>
	button {
		background: none;
		border: none;
		padding: 0;
		font: inherit;
		text-transform: inherit;
		letter-spacing: inherit;
		color: inherit;
		cursor: pointer;
	}
	button:hover {
		color: var(--color-text);
	}
</style>
