<!-- The title of a trend on the creation pages. -->
<script lang="ts">
	import { titleError } from './creation.ts';

	interface Props {
		title: string;
		submitted: boolean;
		placeholder?: string;
	}

	let {
		title = $bindable(),
		submitted,
		placeholder = 'Example: Migration to React function components'
	}: Props = $props();

	let touched = $state(false);
	const error = $derived((touched || submitted) && titleError(title));
</script>

<div class="field">
	<label for="insight-title">Title</label>
	<input
		id="insight-title"
		class="input"
		required
		{placeholder}
		aria-invalid={error ? 'true' : undefined}
		aria-describedby="insight-title-hint"
		bind:value={title}
		onblur={() => (touched = true)}
	/>
	{#if error}
		<span class="error" id="insight-title-hint" role="alert">{error}</span>
	{:else}
		<span class="hint" id="insight-title-hint">Shown as the title for your trend</span>
	{/if}
</div>

<style>
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
</style>
