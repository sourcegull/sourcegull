<script lang="ts">
	import QueryInput from './QueryInput.svelte';
	import type { ContextFormValue } from './context-form.ts';

	interface Props {
		initial: ContextFormValue;
		/** Owners the viewer may pick; omitted when editing (the namespace is fixed). */
		namespaces?: { id: string | null; label: string }[];
		submitLabel: string;
		error?: string | null;
		busy?: boolean;
		onsubmit: (v: ContextFormValue) => void;
		oncancel?: () => void;
	}

	let { initial, namespaces, submitLabel, error = null, busy = false, onsubmit, oncancel }: Props = $props();

	// svelte-ignore state_referenced_locally
	let v = $state({ ...initial });

	function submit(e?: SubmitEvent): void {
		e?.preventDefault();
		onsubmit({ ...v, name: v.name.trim(), description: v.description.trim() });
	}
</script>

<form class="context-form" onsubmit={submit}>
	<div class="row2">
		{#if namespaces}
			<div class="field">
				<label for="ctx-ns">Owner</label>
				<select id="ctx-ns" class="input" bind:value={v.namespace}>
					{#each namespaces as n (n.id ?? '')}
						<option value={n.id}>{n.label}</option>
					{/each}
				</select>
			</div>
		{/if}
		<div class="field grow">
			<label for="ctx-name">Name</label>
			<input id="ctx-name" class="input mono" required pattern="(?:[A-Za-z0-9_.]|-|/)+" bind:value={v.name} />
			<span class="hint">Letters, digits, <code>_ . - /</code>. Used as <code>context:@owner/name</code>.</span>
		</div>
	</div>
	<div class="field">
		<label for="ctx-description">Description</label>
		<input id="ctx-description" class="input" bind:value={v.description} />
	</div>
	<label class="checkbox">
		<input type="checkbox" bind:checked={v.public} /> Public (anyone who can see the owner can use it)
	</label>

	<fieldset class="kind">
		<legend class="label">Defined by</legend>
		<label class="checkbox"><input type="radio" value="query" bind:group={v.kind} /> A query (<code>repo:</code>, <code>file:</code>, <code>lang:</code> and similar filters)</label>
		<label class="checkbox"><input type="radio" value="repositories" bind:group={v.kind} /> A list of repositories and revisions</label>
	</fieldset>

	{#if v.kind === 'query'}
		<div class="field">
			<span class="label">Query</span>
			<QueryInput bind:value={v.query} patternType="keyword" label="Context query" onsubmit={() => submit()} />
			<span class="hint">For example <code>repo:^gitlab\.com/acme/ -file:^vendor/ lang:go</code>.</span>
		</div>
	{:else}
		<div class="field">
			<label for="ctx-repos">Repositories</label>
			<textarea id="ctx-repos" class="input mono" rows="8" bind:value={v.repositories} placeholder="gitlab.com/acme/api&#10;gitlab.com/acme/web@main,release-2.0"></textarea>
			<span class="hint">One per line: <code>name</code> (default branch) or <code>name@rev1,rev2</code>.</span>
		</div>
	{/if}

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	<div class="buttons">
		<button class="btn btn-primary" type="submit" disabled={busy}>{submitLabel}</button>
		{#if oncancel}<button class="btn" type="button" onclick={oncancel}>Cancel</button>{/if}
	</div>
</form>

<style>
	.context-form {
		display: grid;
		gap: var(--space-3);
	}
	.field {
		margin: 0;
	}
	.row2 {
		display: flex;
		gap: var(--space-3);
		flex-wrap: wrap;
	}
	.grow {
		flex: 1;
		min-width: 14rem;
	}
	.kind {
		border: 0;
		padding: 0;
		margin: 0;
		display: grid;
		gap: var(--space-1);
	}
	.buttons {
		display: flex;
		gap: var(--space-2);
	}
</style>
