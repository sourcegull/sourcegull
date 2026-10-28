<!--
	"Export data" of a trend card: a CSV archive of the view's data from
	`/.api/insights/export/{id}`, with only the repositories the viewer may see.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { exportURL } from './charts.ts';

	interface Props {
		id: string;
		title: string;
		onclose: () => void;
	}

	let { id, title, onclose }: Props = $props();

	let dialog: HTMLDialogElement;

	onMount(() => {
		dialog.showModal();
	});
</script>

<dialog
	bind:this={dialog}
	class="card"
	aria-label="Export trend data"
	oncancel={(e) => {
		e.preventDefault();
		onclose();
	}}
>
	<header>
		<h2>Export data for ‘{title}’ trend?</h2>
		<button type="button" class="btn btn-sm" aria-label="Close" onclick={onclose}>✕</button>
	</header>
	<p>This will create a CSV archive of all data for this trend, including data that has been archived.</p>
	<p>This will only include data that you are permitted to see.</p>
	<footer>
		<button type="button" class="btn" onclick={onclose}>Cancel</button>
		<a class="btn btn-primary" href={exportURL(id)} download onclick={() => setTimeout(onclose, 0)}>Export data as CSV</a>
	</footer>
</dialog>

<style>
	dialog {
		width: min(32rem, calc(100vw - 2rem));
		padding: var(--space-4);
		color: var(--color-text);
	}
	dialog::backdrop {
		background: var(--overlay);
	}
	header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--space-2);
	}
	h2 {
		margin: 0;
		font-size: 1.15rem;
		font-weight: 500;
		overflow-wrap: anywhere;
	}
	footer {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-2);
		margin-top: var(--space-4);
	}
</style>
