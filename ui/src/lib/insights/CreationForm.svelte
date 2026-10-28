<!--
	The layout of the trend creation and edit pages: the form fields beside the live
	preview (stacked on narrow screens), and the actions "Cancel", "Clear all fields" (creation
	only) and the submit button. A submit with invalid fields shows every error and focuses the
	first invalid field; a server error is shown above the actions.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import { tick } from 'svelte';
	import { errorMessage } from '#lib/graphql.ts';

	interface Props {
		label: string;
		/** Whether every field is valid. */
		valid: boolean;
		/** Set on the first submit: fields then show their errors untouched. */
		submitted: boolean;
		submitLabel: string;
		submittingLabel: string;
		onsubmit: () => Promise<void>;
		oncancel: () => void;
		onclear?: () => void;
		fields: Snippet;
		preview: Snippet;
	}

	let {
		label,
		valid,
		submitted = $bindable(),
		submitLabel,
		submittingLabel,
		onsubmit,
		oncancel,
		onclear,
		fields,
		preview
	}: Props = $props();

	let form: HTMLFormElement | undefined = $state();
	let submitting = $state(false);
	let error = $state<string | null>(null);

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		submitted = true;
		error = null;
		if (!valid) {
			await tick();
			form?.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus();
			return;
		}
		submitting = true;
		try {
			await onsubmit();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<form class="creation" novalidate aria-label={label} bind:this={form} onsubmit={submit}>
	<div class="layout">
		<div class="fields">{@render fields()}</div>
		<aside class="preview">{@render preview()}</aside>
	</div>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	<div class="actions">
		<button type="button" class="btn" onclick={oncancel}>Cancel</button>
		{#if onclear}
			<button type="button" class="btn" onclick={onclear}>Clear all fields</button>
		{/if}
		<button type="submit" class="btn btn-primary" disabled={submitting}>
			{submitting ? submittingLabel : submitLabel}
		</button>
	</div>
</form>

<style>
	.creation {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: var(--space-5, 2rem);
		align-items: start;
	}
	.fields {
		display: flex;
		flex-direction: column;
		gap: var(--space-5, 2rem);
		min-width: 0;
	}
	.preview {
		position: sticky;
		/* Below the sticky top navigation. */
		top: 4rem;
		min-width: 0;
	}
	@media (max-width: 900px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}
		.preview {
			position: static;
		}
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		border-top: 1px solid var(--color-border);
		padding-top: var(--space-4);
	}
	.actions .btn-primary {
		margin-left: auto;
	}
</style>
