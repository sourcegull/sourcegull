<!--
	The spec editor of a bulk change: the YAML
	editor with debounced validation (`createBatchSpecFromRaw` for a new spec,
	`replaceBatchSpecInput` while the spec has not run), issues with line and column, a library
	of examples, the workspace preview, and running the spec on the server (`executeBatchSpec`).
-->
<script lang="ts">
	import type { CreateBatchSpecFromRawMutation, ReplaceBatchSpecInputMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import { onDestroy, onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import SpecEditor from './SpecEditor.svelte';
	import WorkspacesPreview from './WorkspacesPreview.svelte';
	import {
		BATCH_CHANGE_FOR_EDIT,
		CREATE_BATCH_SPEC_FROM_RAW,
		EDITOR_SPEC,
		EXECUTE_BATCH_SPEC,
		ORG_NAMESPACE,
		REPLACE_BATCH_SPEC_INPUT,
		SPEC_EXAMPLES,
		USER_NAMESPACE,
		canRun,
		executionURL,
		formatIssue,
		initialSpec,
		isEmptySpec,
		isResolving,
		isSpecExecuted,
		latestSpec,
		loadBatchesAccess,
		specIssues,
		specName,
		specSaveMode,
		withSpecName,
		type BatchChangeForEditResult,
		type BatchesAccess,
		type EditorBatchChange,
		type EditorSpec,
		type EditorSpecResult,
		type NamespaceKind,
		type NamespaceResult,
		type SpecFlags,
		type SpecIssue
	} from './spec.ts';

	interface Props {
		kind: NamespaceKind;
		namespace: string;
		name: string;
	}

	let { kind, namespace, name }: Props = $props();

	/** Validation waits this long after the last keystroke. */
	const DEBOUNCE_MS = 800;
	/** The resolution is polled this often while it runs. */
	const POLL_MS = 1500;

	let access = $state.raw<BatchesAccess | null>(null);
	let namespaceId = $state<string | null>(null);
	let bc = $state.raw<EditorBatchChange | null>(null);
	let spec = $state.raw<EditorSpec | null>(null);
	let code = $state('');
	let issues = $state.raw<SpecIssue[]>([]);
	let saving = $state(false);
	let saveError = $state<string | null>(null);
	let loadError = $state<string | null>(null);
	let notFound = $state(false);
	let noCache = $state(false);
	let running = $state(false);
	let runError = $state<string | null>(null);
	let example = $state('');
	let editor = $state<SpecEditor | undefined>();
	/** The text whose validation failed (issues apply to it). */
	let failedCode = $state<string | null>(null);

	const editable = $derived(
		access?.available === true && access.canWrite && bc?.viewerCanAdminister === true && bc.state !== 'CLOSED'
	);
	/** The text differs from the last valid spec and has not failed validation as is. */
	const dirty = $derived(spec !== null && code !== spec.originalInput && code !== failedCode);
	const nameMismatch = $derived.by(() => {
		const n = specName(code);
		return bc && n !== null && n !== bc.name ? n : null;
	});
	const executed = $derived(spec !== null && isSpecExecuted(spec));
	const runnable = $derived(
		editable && spec !== null && !dirty && !saving && issues.length === 0 && !executed && canRun(spec) && nameMismatch === null
	);

	let debounce: ReturnType<typeof setTimeout> | undefined;
	let poll: ReturnType<typeof setTimeout> | undefined;
	let destroyed = false;

	async function load(): Promise<void> {
		loadError = null;
		try {
			access = await loadBatchesAccess();
			if (!access.available) return;
			const ns = await graphql<NamespaceResult, { name: string }>(
				kind === 'users' ? USER_NAMESPACE : ORG_NAMESPACE,
				{ name: namespace }
			);
			namespaceId = (kind === 'users' ? ns.user?.id : ns.organization?.id) ?? null;
			if (!namespaceId) {
				notFound = true;
				return;
			}
			const r = await graphql<BatchChangeForEditResult, { namespace: string; name: string }>(BATCH_CHANGE_FOR_EDIT, {
				namespace: namespaceId,
				name
			});
			if (!r.batchChange) {
				notFound = true;
				return;
			}
			bc = r.batchChange;
			spec = latestSpec(r.batchChange);
			code = isEmptySpec(spec.originalInput) && !isSpecExecuted(spec) ? initialSpec(bc.name) : spec.originalInput;
			schedulePoll();
		} catch (err) {
			loadError = errorMessage(err);
		}
	}

	/** Polls the spec while its workspaces are being resolved. */
	function schedulePoll(): void {
		clearTimeout(poll);
		if (destroyed || !spec || !isResolving(spec.workspaceResolution?.state)) return;
		poll = setTimeout(async () => {
			const id = spec?.id;
			if (!id) return;
			try {
				const r = await graphql<EditorSpecResult, { id: string }>(EDITOR_SPEC, { id });
				if (r.node?.__typename === 'BatchSpec' && spec?.id === id && !saving) spec = r.node as EditorSpec;
			} catch {
				// Try again on the next tick.
			}
			schedulePoll();
		}, POLL_MS);
	}

	/** Saves the text (and flags) as the spec: validated by the server, workspaces resolved again. */
	async function save(flags?: Pick<SpecFlags, 'allowIgnored' | 'allowUnsupported'>): Promise<void> {
		if (!bc || !spec || !namespaceId || !editable) return;
		if (saving) {
			// Another save follows this one (the effect sees `dirty`).
			return;
		}
		const text = code;
		const allowIgnored = flags?.allowIgnored ?? spec.allowIgnored === true;
		const allowUnsupported = flags?.allowUnsupported ?? spec.allowUnsupported === true;
		saving = true;
		saveError = null;
		try {
			let next: EditorSpec;
			if (specSaveMode(spec, bc) === 'replace') {
				const r = await graphql<ReplaceBatchSpecInputMutation, Record<string, unknown>>(REPLACE_BATCH_SPEC_INPUT, {
					previousSpec: spec.id,
					spec: text,
					allowIgnored,
					allowUnsupported
				});
				next = r.replaceBatchSpecInput;
			} else {
				const r = await graphql<CreateBatchSpecFromRawMutation, Record<string, unknown>>(
					CREATE_BATCH_SPEC_FROM_RAW,
					{ spec: text, namespace: namespaceId, batchChange: bc.id, allowIgnored, allowUnsupported }
				);
				next = r.createBatchSpecFromRaw;
			}
			spec = next;
			issues = [];
			failedCode = null;
		} catch (err) {
			const found = specIssues(err);
			if (found) {
				issues = found;
				failedCode = text;
			} else {
				saveError = errorMessage(err);
				failedCode = text;
			}
		} finally {
			saving = false;
		}
		schedulePoll();
		// The text changed while saving: validate the newest text.
		if (code !== text && code !== spec?.originalInput) scheduleSave();
	}

	function scheduleSave(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => void save(), DEBOUNCE_MS);
	}

	// Validate after each edit (debounced). Issues stay until the next validation.
	$effect(() => {
		void code; // restart the debounce on every keystroke
		if (dirty && editable) scheduleSave();
	});

	function insertExample(): void {
		const ex = SPEC_EXAMPLES.find((e) => e.id === example);
		if (!ex || !bc) return;
		if (code.trim() !== '' && !confirm('Replace the spec in the editor with this example?')) return;
		code = withSpecName(ex.code, bc.name);
		example = '';
	}

	async function run(): Promise<void> {
		if (!spec || !bc || !runnable) return;
		running = true;
		runError = null;
		try {
			await graphql<unknown, { batchSpec: string; noCache: boolean }>(EXECUTE_BATCH_SPEC, {
				batchSpec: spec.id,
				noCache
			});
			await goto(executionURL(bc.url, spec.id));
		} catch (err) {
			runError = errorMessage(err);
		} finally {
			running = false;
		}
	}

	onMount(() => void load());
	onDestroy(() => {
		destroyed = true;
		clearTimeout(debounce);
		clearTimeout(poll);
	});
</script>

<svelte:head><title>Edit spec · {name} · Bulk changes · Sourcegull</title></svelte:head>

<div class="edit-page">
	<nav class="crumbs muted" aria-label="Breadcrumbs">
		<a href="/bulk-changes">Bulk changes</a> / {namespace} /
		{#if bc}<a href={bc.url}>{name}</a>{:else}{name}{/if}
	</nav>
	<div class="page-header">
		<h1>{name}</h1>
		{#if bc}<span class="badge">{bc.state.toLowerCase()}</span>{/if}
		<span class="spacer"></span>
		{#if bc && editable}
			<div class="run">
				<label class="checkbox small" title="Run every step again, ignoring cached results">
					<input type="checkbox" bind:checked={noCache} />
					Disable cache
				</label>
				<button class="btn btn-primary" type="button" disabled={!runnable || running} onclick={run}>
					{running ? 'Starting…' : 'Run bulk change spec'}
				</button>
			</div>
		{/if}
	</div>

	{#if loadError}
		<div class="alert alert-danger" role="alert">{loadError}</div>
	{:else if access && !access.available}
		<div class="alert alert-warning" role="alert">{access.message}</div>
	{:else if notFound}
		<div class="alert alert-warning" role="alert">Bulk change not found.</div>
	{:else if !bc || !spec}
		<p class="muted">Loading…</p>
	{:else}
		{#if !editable}
			<div class="alert alert-info" role="status">
				{#if bc.state === 'CLOSED'}
					This bulk change is closed; its spec cannot be changed.
				{:else}
					You can view this spec, but not change or run it.
				{/if}
			</div>
		{/if}
		{#if executed && spec}
			<div class="alert alert-info" role="status">
				This spec has run ({spec.state.toLowerCase()}).
				<a href={executionURL(bc.url, spec.id)}>View the execution</a>. Editing it creates a new spec.
			</div>
		{/if}
		{#if runError}<div class="alert alert-danger" role="alert">{runError}</div>{/if}
		{#if saveError}<div class="alert alert-danger" role="alert">{saveError}</div>{/if}
		{#if nameMismatch}
			<div class="alert alert-warning" role="alert">
				The spec's name <code>{nameMismatch}</code> differs from the bulk change's name
				<code>{bc.name}</code>. Change it back to run the spec.
			</div>
		{/if}

		<div class="layout">
			<section class="editor-col" aria-label="Bulk change spec">
				<div class="editor-bar">
					{#if editable}
						<label class="visually-hidden" for="spec-example">Examples</label>
						<select id="spec-example" class="input example" bind:value={example}>
							<option value="">Start from an example…</option>
							{#each SPEC_EXAMPLES as ex (ex.id)}
								<option value={ex.id} title={ex.description}>{ex.title}</option>
							{/each}
						</select>
						<button class="btn btn-sm" type="button" disabled={example === ''} onclick={insertExample}>Insert</button>
					{/if}
					<span class="spacer"></span>
					<span class="validation small" role="status" data-state={saving || dirty ? 'pending' : issues.length > 0 ? 'invalid' : 'valid'}>
						{#if saving || dirty}
							Validating…
						{:else if issues.length > 0}
							{issues.length} {issues.length === 1 ? 'error' : 'errors'}
						{:else if !saveError}
							Valid spec
						{/if}
					</span>
				</div>
				<SpecEditor bind:this={editor} bind:value={code} {issues} readOnly={!editable} onsave={() => void save()} />
				{#if issues.length > 0}
					<ul class="issues" aria-label="Spec errors">
						{#each issues as issue, i (i)}
							<li>
								<button class="btn-link issue" type="button" onclick={() => editor?.reveal(issue)}>{formatIssue(issue)}</button>
							</li>
						{/each}
					</ul>
				{/if}
				<p class="muted small help">
					Steps run in containers in each workspace; <code>{'${{ … }}'}</code> templates can use
					<code>repository.name</code>, <code>steps.modified_files</code>, <code>outputs.&lt;name&gt;</code> and more.
				</p>
			</section>
			<WorkspacesPreview
				{spec}
				stale={dirty || saving}
				editable={editable && !executed}
				onflags={(f) => void save(f)}
			/>
		</div>
	{/if}
</div>

<style>
	.edit-page {
		max-width: 96rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
	}
	.crumbs {
		margin-bottom: var(--space-2);
		overflow-wrap: anywhere;
	}
	.page-header {
		flex-wrap: wrap;
		justify-content: flex-start;
	}
	.page-header h1 {
		overflow-wrap: anywhere;
	}
	.spacer {
		flex: 1;
	}
	.run {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}
	.small {
		font-size: 0.875em;
	}
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
		gap: var(--space-6);
		align-items: start;
	}
	.editor-col {
		min-width: 0;
	}
	.editor-bar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-bottom: var(--space-2);
		flex-wrap: wrap;
	}
	.example {
		width: auto;
		max-width: 100%;
	}
	.validation[data-state='invalid'] {
		color: var(--color-danger);
		font-weight: 600;
	}
	.validation[data-state='valid'] {
		color: var(--color-success);
	}
	.issues {
		margin: var(--space-2) 0 0;
		padding-left: 1.25rem;
		color: var(--color-danger);
	}
	.issue {
		text-align: left;
		white-space: normal;
		font: inherit;
		color: var(--color-danger);
		overflow-wrap: anywhere;
	}
	.help {
		margin-top: var(--space-2);
	}
	@media (max-width: 60rem) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
