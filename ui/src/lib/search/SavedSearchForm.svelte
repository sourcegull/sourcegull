<script lang="ts" module>
	export interface SavedSearchFields {
		description: string;
		query: string;
		draft: boolean;
	}
</script>

<script lang="ts">
	import QueryInput from './QueryInput.svelte';
	import { parsePatternType } from './query.ts';

	interface Props {
		initial: SavedSearchFields;
		submitLabel: string;
		error?: string | null;
		busy?: boolean;
		onsubmit: (v: SavedSearchFields) => void;
		oncancel?: () => void;
	}

	let { initial, submitLabel, error = null, busy = false, onsubmit, oncancel }: Props = $props();

	// The form edits a copy; `initial` only seeds it.
	// svelte-ignore state_referenced_locally
	let description = $state(initial.description);
	// svelte-ignore state_referenced_locally
	let query = $state(initial.query);
	// svelte-ignore state_referenced_locally
	let draft = $state(initial.draft);

	const patternType = $derived(parsePatternType(/patterntype:(\S+)/i.exec(query)?.[1]));
	const missingPatternType = $derived(!/(^|\s)patterntype:/i.test(query));

	function submit(e?: SubmitEvent): void {
		e?.preventDefault();
		onsubmit({ description: description.trim(), query: query.trim(), draft });
	}
</script>

<form class="saved-form" onsubmit={submit}>
	<div class="field">
		<label for="ss-description">Description</label>
		<input id="ss-description" class="input" required bind:value={description} />
	</div>
	<div class="field">
		<span class="label">Query</span>
		<QueryInput bind:value={query} {patternType} label="Saved search query" onsubmit={() => submit()} />
		<span class="hint" class:warn={missingPatternType}>
			Must contain a <code>patternType:</code> (keyword, standard, literal or regexp).
		</span>
	</div>
	<label class="checkbox">
		<input type="checkbox" bind:checked={draft} /> Draft (not ready to share yet)
	</label>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	<div class="row">
		<button class="btn btn-primary" type="submit" disabled={busy}>{submitLabel}</button>
		{#if oncancel}<button class="btn" type="button" onclick={oncancel}>Cancel</button>{/if}
	</div>
</form>

<style>
	.saved-form {
		display: grid;
		gap: var(--space-3);
	}
	.field {
		display: grid;
		gap: var(--space-1);
		margin: 0;
	}
	.hint.warn {
		color: var(--color-warning);
	}
	.row {
		display: flex;
		gap: var(--space-2);
	}
</style>
