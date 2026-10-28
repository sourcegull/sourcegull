<!--
	Create or edit a code host connection: display name and the JSONC configuration, as a form
	(the default) or as JSON. Both edit the same text: the form changes only the values it
	touches, so comments and settings it doesn't show survive. The text is validated live
	against the code host's connection schema (`validateExternalServiceConfig`) and can be
	checked against the code host before saving (`checkExternalServiceConfig`). The server
	validates again on save; its errors are shown the same way. Secrets in an existing
	configuration read `REDACTED`; leaving them so keeps the stored ones.
-->
<script lang="ts">
	import JsoncEditor from '#lib/components/JsoncEditor.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
	import GiteaConnectionFields from './GiteaConnectionFields.svelte';
	import GitHubConnectionFields from './GitHubConnectionFields.svelte';
	import GitLabConnectionFields from './GitLabConnectionFields.svelte';
	import type { Edit } from './gitlab-form.ts';
	import {
		CHECK_CONFIG,
		VALIDATE_CONFIG,
		type CheckConfigResult,
		type ConfigCheck,
		kindLabel,
		type ExternalServiceKind,
		type ValidateConfigResult
	} from './queries.ts';

	interface Props {
		kind: ExternalServiceKind;
		/** The connection being edited; its stored secrets fill in `REDACTED` when testing. */
		serviceId?: string;
		initialName: string;
		initialConfig: string;
		submitLabel: string;
		busy?: boolean;
		/** A save error: its message, and the schema problems for VALIDATION errors. */
		error?: string | null;
		errors?: string[] | null;
		onsubmit: (displayName: string, config: string) => void;
		oncancel?: () => void;
		/** Called with whether the form differs from its initial values. */
		ondirty?: (dirty: boolean) => void;
	}

	let {
		kind,
		serviceId,
		initialName,
		initialConfig,
		submitLabel,
		busy = false,
		error = null,
		errors = null,
		onsubmit,
		oncancel,
		ondirty
	}: Props = $props();

	// The initial values seed the form once; later prop changes do not reset edits.
	// svelte-ignore state_referenced_locally
	let displayName = $state(initialName);
	// svelte-ignore state_referenced_locally
	let config = $state(initialConfig);

	let problems = $state<string[]>([]);
	let warnings = $state<string[]>([]);
	let validating = $state(false);
	let validateError = $state<string | null>(null);
	/** Increments per request so a slow answer for older text is dropped. */
	let generation = 0;

	const label = $derived(kindLabel(kind));
	const dirty = $derived(displayName !== initialName || config !== initialConfig);
	$effect(() => ondirty?.(dirty));

	async function validate(text: string): Promise<void> {
		const mine = ++generation;
		validating = true;
		try {
			const r = await graphql<ValidateConfigResult, { kind: ExternalServiceKind; config: string }>(
				VALIDATE_CONFIG,
				{ kind, config: text }
			);
			if (mine !== generation) return;
			problems = r.validateExternalServiceConfig.errors;
			warnings = r.validateExternalServiceConfig.warnings;
			validateError = null;
		} catch (err) {
			if (mine !== generation) return;
			validateError = errorMessage(err);
		} finally {
			if (mine === generation) validating = false;
		}
	}

	// Validate 400 ms after the last keystroke.
	$effect(() => {
		const text = config;
		const timer = setTimeout(() => void validate(text), 400);
		return () => clearTimeout(timer);
	});

	let view = $state<'form' | 'json'>('form');
	/** The configuration parsed; null while the JSON has a syntax error. */
	const doc = $derived(parseJsonc(config));

	function edit(edits: Edit[]): void {
		let text = config;
		for (const [path, value] of edits) text = setAt(text, path, value);
		config = text;
	}

	let checking = $state(false);
	let checkResult = $state<ConfigCheck | null>(null);
	let checkError = $state<string | null>(null);
	/** The text the last check ran on; the result is hidden once the text changes. */
	let checkedConfig = $state<string | null>(null);

	async function testConnection(): Promise<void> {
		checking = true;
		checkError = null;
		const text = config;
		try {
			const r = await graphql<CheckConfigResult, { kind: ExternalServiceKind; config: string; id: string | null }>(
				CHECK_CONFIG,
				{ kind, config: text, id: serviceId ?? null }
			);
			checkResult = r.checkExternalServiceConfig;
		} catch (err) {
			checkResult = null;
			checkError = errorMessage(err);
		} finally {
			checkedConfig = text;
			checking = false;
		}
	}

	function submit(event?: SubmitEvent): void {
		event?.preventDefault();
		if (busy) return;
		onsubmit(displayName.trim(), config);
	}
</script>

<form class="connection-form card" onsubmit={submit}>
	<div class="field">
		<label for="connection-name">Display name</label>
		<input id="connection-name" class="input" bind:value={displayName} required maxlength="200" />
	</div>

	<div class="tabs" role="tablist" aria-label="Configuration editor">
		<button
			class="tab"
			type="button"
			role="tab"
			id="connection-tab-form"
			aria-selected={view === 'form'}
			aria-controls="connection-config"
			onclick={() => (view = 'form')}
		>
			Settings
		</button>
		<button
			class="tab"
			type="button"
			role="tab"
			id="connection-tab-json"
			aria-selected={view === 'json'}
			aria-controls="connection-config"
			onclick={() => (view = 'json')}
		>
			Edit as JSON
		</button>
	</div>

	<div
		id="connection-config"
		role="tabpanel"
		aria-labelledby={view === 'form' ? 'connection-tab-form' : 'connection-tab-json'}
	>
		{#if view === 'json'}
			<div class="field">
				<span class="hint">
					JSON with comments, checked against the {label} connection schema as you type. Secrets show as
					<code>REDACTED</code>; leave them so to keep the stored values. Press <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd
						>S</kbd
					> to save.
				</span>
				<JsoncEditor bind:value={config} label="Connection configuration (JSON with comments)" onsave={submit} />
			</div>
		{:else if doc === null}
			<div class="alert alert-warning" role="status">
				The JSON has a syntax error, so the form can't show it.
				<button type="button" class="btn btn-link" onclick={() => (view = 'json')}>Fix it in the JSON editor</button>
			</div>
		{:else if kind === 'GITHUB'}
			<GitHubConnectionFields
				{doc}
				editing={serviceId !== undefined}
				disabled={busy}
				onedit={edit}
				onjson={() => (view = 'json')}
			/>
		{:else if kind === 'GITEA'}
			<GiteaConnectionFields
				{doc}
				editing={serviceId !== undefined}
				disabled={busy}
				onedit={edit}
				onjson={() => (view = 'json')}
			/>
		{:else}
			<GitLabConnectionFields
				{doc}
				editing={serviceId !== undefined}
				disabled={busy}
				onedit={edit}
				onjson={() => (view = 'json')}
			/>
		{/if}
	</div>

	<div class="validation" aria-live="polite" data-testid="config-validation">
		{#if validateError}
			<div class="alert alert-warning">Could not validate: {validateError}</div>
		{:else if problems.length > 0}
			<div class="alert alert-danger">
				<strong>{problems.length} problem{problems.length === 1 ? '' : 's'}:</strong>
				<ul>
					{#each problems as p, i (i)}<li>{p}</li>{/each}
				</ul>
			</div>
		{:else if !validating}
			<p class="valid small">The configuration is valid.</p>
		{/if}
		{#if warnings.length > 0}
			<div class="alert alert-warning">
				<ul>
					{#each warnings as w, i (i)}<li>{w}</li>{/each}
				</ul>
			</div>
		{/if}
	</div>

	{#if error}
		<div class="alert alert-danger" role="alert">
			<strong>Save failed.</strong>
			{#if errors && errors.length > 0}
				<ul>
					{#each errors as e, i (i)}<li>{e}</li>{/each}
				</ul>
			{:else}
				<pre class="error-text">{error}</pre>
			{/if}
		</div>
	{/if}

	<div class="check" aria-live="polite" data-testid="config-check">
		<button class="btn" type="button" onclick={testConnection} disabled={checking || busy || problems.length > 0}>
			{checking ? 'Testing…' : 'Test connection'}
		</button>
		{#if checkedConfig === config && !checking}
			{#if checkError}
				<span class="check-bad">Could not test: {checkError}</span>
			{:else if checkResult?.ok}
				<span class="check-ok">
					Connected{checkResult.account ? ` as ${checkResult.account}` : ''}{checkResult.scopes
						? ` · token scopes: ${checkResult.scopes.join(', ') || 'none'}`
						: ''}.
				</span>
			{:else if checkResult}
				<span class="check-bad">{checkResult.problem}</span>
			{/if}
		{/if}
	</div>
	{#if checkedConfig === config && !checking && checkResult?.ok && checkResult.warnings.length > 0}
		<div class="alert alert-warning">
			<ul>
				{#each checkResult.warnings as w, i (i)}<li>{w}</li>{/each}
			</ul>
		</div>
	{/if}

	<div class="toolbar">
		<button class="btn btn-primary" type="submit" disabled={busy || !displayName.trim()}>
			{busy ? 'Saving…' : submitLabel}
		</button>
		{#if oncancel}
			<button class="btn" type="button" onclick={oncancel} disabled={busy}>Cancel</button>
		{/if}
	</div>
</form>

<style>
	.validation {
		margin-bottom: var(--space-3);
		min-height: 1.5em;
	}
	.valid {
		color: var(--color-success);
		margin: 0;
	}
	.connection-form {
		max-width: 60rem;
	}
	.error-text {
		margin: var(--space-1) 0 0;
		white-space: pre-wrap;
		font-size: var(--text-sm);
	}
	.check {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2) var(--space-3);
		margin-bottom: var(--space-3);
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
	}
	.check-ok {
		color: var(--color-success);
	}
	.check-bad {
		color: var(--color-danger);
	}
	.toolbar {
		padding-top: var(--space-4);
		border-top: 1px solid var(--color-border);
	}
</style>
