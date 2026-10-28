<script lang="ts">
	import { errorMessage } from '#lib/graphql.ts';
	import { orgTitle, visibilityKey, type Visibility } from './insights.ts';
	import type { OrgRef } from './queries.ts';

	interface Props {
		name?: string;
		visibility?: Visibility;
		orgs: OrgRef[];
		submitLabel: string;
		submittingLabel: string;
		onsubmit: (name: string, visibility: Visibility) => Promise<void>;
		oncancel: () => void;
	}

	let {
		name: initialName = '',
		visibility: initialVisibility = { kind: 'personal' },
		orgs,
		submitLabel,
		submittingLabel,
		onsubmit,
		oncancel
	}: Props = $props();

	// svelte-ignore state_referenced_locally
	let name = $state(initialName);
	// svelte-ignore state_referenced_locally
	let visibility = $state<Visibility>(initialVisibility);
	let touched = $state(false);
	let submitting = $state(false);
	let error = $state<string | null>(null);

	const nameError = $derived(name.trim() === '' ? 'Name is a required field.' : null);

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		touched = true;
		if (nameError) return;
		submitting = true;
		error = null;
		try {
			await onsubmit(name.trim(), visibility);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<form class="card" novalidate onsubmit={submit}>
	<div class="field">
		<label for="dashboard-name">Name</label>
		<!-- svelte-ignore a11y_autofocus -->
		<input
			id="dashboard-name"
			class="input"
			required
			autofocus
			placeholder="Example: My personal code trends dashboard"
			aria-invalid={touched && nameError ? 'true' : undefined}
			aria-describedby="dashboard-name-hint"
			bind:value={name}
			onblur={() => (touched = true)}
		/>
		{#if touched && nameError}
			<span class="error" id="dashboard-name-hint">{nameError}</span>
		{:else}
			<span class="hint" id="dashboard-name-hint">Shown as the title for your dashboard</span>
		{/if}
	</div>

	<fieldset class="visibility">
		<legend class="label">Visibility</legend>
		<label class="radio">
			<input
				type="radio"
				name="visibility"
				value="personal"
				checked={visibility.kind === 'personal'}
				onchange={() => (visibility = { kind: 'personal' })}
			/>
			<span><strong>Private</strong> <span class="muted">visible only to you</span></span>
		</label>
		<hr />
		<small class="muted">Shared - visible to everyone in the chosen organization</small>
		{#each orgs as org (org.id)}
			<label class="radio">
				<input
					type="radio"
					name="visibility"
					value={`org:${org.id}`}
					checked={visibilityKey(visibility) === `org:${org.id}`}
					onchange={() => (visibility = { kind: 'organization', id: org.id })}
				/>
				<strong>{orgTitle(org)}</strong>
			</label>
		{:else}
			<label class="radio" title="Create or join an organization to share the dashboard with others!">
				<input type="radio" name="visibility" value="organization" disabled />
				<span><strong>Organization</strong> <span class="muted">all users in your organization</span></span>
			</label>
		{/each}
		<label class="radio">
			<input
				type="radio"
				name="visibility"
				value="global"
				checked={visibility.kind === 'global'}
				onchange={() => (visibility = { kind: 'global' })}
			/>
			<span><strong>Global</strong> <span class="muted">visible to everyone on this Sourcegull instance</span></span>
		</label>
	</fieldset>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	<div class="actions">
		<button type="button" class="btn" onclick={oncancel}>Cancel</button>
		<button type="submit" class="btn btn-primary" disabled={submitting}>
			{submitting ? submittingLabel : submitLabel}
		</button>
	</div>
</form>

<style>
	.visibility {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.visibility legend {
		margin-bottom: var(--space-2);
	}
	.radio {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
	}
	hr {
		width: 100%;
		border: 0;
		border-top: 1px solid var(--color-border);
		margin: var(--space-1) 0;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
	.alert {
		margin-top: var(--space-3);
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-top: var(--space-4);
	}
</style>
