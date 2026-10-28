<!--
	Create or edit an outbound webhook: URL and HMAC secret (both write-only; the URL is read
	back masked) and event types with optional scopes.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		OUTBOUND_WEBHOOK_EVENT_TYPES,
		eventTypesInput,
		validateWebhookForm,
		type EventTypeInfo,
		type EventTypesResult,
		type ScopedEventType
	} from '#lib/admin/outbound-webhooks.ts';

	interface Props {
		/** The webhook being edited, with its masked URL; absent for a new one. */
		initial?: { url: string; eventTypes: ScopedEventType[] };
		submitLabel: string;
		onsubmit: (input: { url: string; secret: string | null; eventTypes: ScopedEventType[] }) => Promise<void>;
	}

	let { initial, submitLabel, onsubmit }: Props = $props();

	const isNew = $derived(!initial);

	let eventTypes = $state.raw<EventTypeInfo[]>([]);
	let url = $state('');
	let secret = $state('');
	let checked = $state<Record<string, boolean>>({});
	let scopes = $state<Record<string, string>>({});
	let problems = $state<string[]>([]);
	let error = $state<string | null>(null);
	let saving = $state(false);

	$effect.pre(() => {
		url = '';
		checked = Object.fromEntries((initial?.eventTypes ?? []).map((t) => [t.eventType, true]));
		scopes = Object.fromEntries((initial?.eventTypes ?? []).map((t) => [t.eventType, t.scope ?? '']));
	});

	onMount(async () => {
		try {
			eventTypes = (await graphql<EventTypesResult>(OUTBOUND_WEBHOOK_EVENT_TYPES)).outboundWebhookEventTypes;
		} catch (err) {
			error = errorMessage(err);
		}
	});

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		const types = eventTypesInput(checked, scopes);
		problems = validateWebhookForm(url, secret, isNew, types);
		if (problems.length > 0) return;
		saving = true;
		error = null;
		try {
			await onsubmit({ url: url.trim(), secret: secret === '' ? null : secret, eventTypes: types });
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}
</script>

<form class="card webhook-form" onsubmit={submit}>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#if problems.length > 0}
		<div class="alert alert-warning" role="alert">
			<ul>
				{#each problems as p (p)}<li>{p}</li>{/each}
			</ul>
		</div>
	{/if}
	<label class="field">
		<span class="label">URL</span>
		<input
			class="input"
			type="url"
			bind:value={url}
			required={isNew}
			placeholder={initial?.url || 'https://hooks.example.com/sourcegull'}
		/>
		<span class="muted small"
			>Addresses on internal networks are refused unless listed in
			<code>webhooks.outbound.allowedHosts</code>.{#if !isNew}
				Leave empty to keep the stored URL.{/if}</span
		>
	</label>
	<label class="field">
		<span class="label">Secret</span>
		<input
			class="input"
			type="password"
			autocomplete="new-password"
			bind:value={secret}
			placeholder={isNew ? '' : 'Unchanged'}
			required={isNew}
		/>
		<span class="muted small"
			>Each delivery carries an HMAC-SHA256 of its body with this secret in
			<code>X-Sourcegull-Webhook-Signature</code>. It cannot be read back.</span
		>
	</label>
	<fieldset class="field">
		<legend class="label">Event types</legend>
		{#each eventTypes as t (t.key)}
			<div class="event-type">
				<label class="checkbox">
					<input
						type="checkbox"
						checked={checked[t.key] ?? false}
						onchange={(e) => (checked = { ...checked, [t.key]: e.currentTarget.checked })}
					/>
					<span class="mono">{t.key}</span>
					<span class="muted small">{t.description}</span>
				</label>
				{#if checked[t.key]}
					<input
						class="input scope"
						placeholder="Scope (optional)"
						aria-label="Scope of {t.key}"
						value={scopes[t.key] ?? ''}
						oninput={(e) => (scopes = { ...scopes, [t.key]: e.currentTarget.value })}
					/>
				{/if}
			</div>
		{:else}
			<p class="muted">Loading event types…</p>
		{/each}
	</fieldset>
	<button class="btn btn-primary" type="submit" disabled={saving}>{saving ? 'Saving…' : submitLabel}</button>
</form>

<style>
	.webhook-form {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		max-width: 48rem;
	}
	fieldset {
		border: 0;
		padding: 0;
		margin: 0;
	}
	.event-type {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) 0;
	}
	.scope {
		max-width: 16rem;
	}
	.webhook-form > .btn {
		align-self: flex-start;
	}
	.small {
		font-size: var(--text-xs);
	}
</style>
