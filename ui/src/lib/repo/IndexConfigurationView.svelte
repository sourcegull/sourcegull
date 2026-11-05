<script lang="ts">
	import type { QueueAutoIndexJobsForRepoMutation } from '#lib/graphql-types.ts';
	import SaveBar from '#lib/components/SaveBar.svelte';
	import ConfigField from '#lib/components/site-config/ConfigField.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
	import { requiredIssues, type ConfigPath } from '#lib/site-config-form.ts';
	import {
		INDEX_CONFIGURATION,
		QUEUE_AUTO_INDEX_JOBS,
		UPDATE_INDEX_CONFIGURATION,
		displayRoot,
		type IndexConfigurationResult,
		type InferredConfiguration
	} from './codegraph.ts';
	import { indexJobsField } from './index-config-form.ts';

	interface Props {
		/** The repository's node ID. */
		repoId: string;
		siteAdmin: boolean;
		/** Called after jobs were queued. */
		onqueued?: () => void;
	}

	let { repoId, siteAdmin, onqueued }: Props = $props();

	let saved = $state<string | null>(null);
	let text = $state('');
	let inferred = $state.raw<InferredConfiguration | null>(null);
	let loading = $state(true);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);

	let view = $state<'jobs' | 'json'>('jobs');
	const jobsField = indexJobsField();
	/** Both views edit `text`; null while it has a syntax error. */
	const doc = $derived(parseJsonc(text));
	const savedDoc = $derived(parseJsonc(saved ?? ''));
	/** A saved configuration without jobs runs none: it still takes precedence over the others. */
	const runsNoJobs = $derived(text.trim() !== '' && doc !== null && !(doc.index_jobs as unknown[] | undefined)?.length);
	const missing = $derived(doc ? requiredIssues([jobsField], doc) : []);
	/** Whether a save was refused for empty required values; they are marked from then on. */
	let showRequired = $state(false);

	const dirty = $derived(text !== (saved ?? ''));
	let saving = $state(false);
	let savedAt = $state<number | null>(null);

	function discard(): void {
		text = saved ?? '';
		error = null;
		showRequired = false;
	}

	function setValue(path: ConfigPath, value: unknown): void {
		text = setAt(text, path, value);
		notice = null;
	}

	$effect(() => {
		void load(repoId);
	});

	async function load(repo: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<IndexConfigurationResult, { repo: string }>(INDEX_CONFIGURATION, { repo });
			const c = r.node?.indexConfiguration ?? null;
			saved = c?.configuration ?? null;
			inferred = c?.inferredConfiguration ?? null;
			text = saved ?? '';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function save(): Promise<void> {
		showRequired = true;
		if (missing.length > 0) {
			view = 'jobs';
			notice = null;
			error = null;
			return;
		}
		busy = true;
		saving = true;
		error = null;
		notice = null;
		try {
			await graphql(UPDATE_INDEX_CONFIGURATION, { repo: repoId, configuration: text });
			saved = text.trim() ? text : null;
			text = saved ?? '';
			savedAt = Date.now();
			if (!saved) notice = 'Removed the configuration.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
			saving = false;
		}
	}

	/** Deletes the saved configuration, so `sourcegull.yaml` or inference applies again. */
	async function remove(): Promise<void> {
		text = '';
		await save();
	}

	async function queue(): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			const r = await graphql<
				QueueAutoIndexJobsForRepoMutation,
				{ repo: string; rev: string | null; configuration: string | null }
			>(QUEUE_AUTO_INDEX_JOBS, { repo: repoId, rev: null, configuration: null });
			const jobs = r.queueAutoIndexJobsForRepo;
			notice =
				jobs.length === 0
					? 'No index jobs to queue.'
					: `Queued ${jobs.length} index ${jobs.length === 1 ? 'job' : 'jobs'} (${jobs
							.map((j) => displayRoot(j.inputRoot))
							.join(', ')}).`;
			onqueued?.();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<div class="index-configuration">
	<div class="page-header">
		<h2>Index configuration</h2>
	</div>
	<p class="muted intro">
		Auto-indexing runs the jobs of this configuration if one is saved, else those of <code>sourcegull.yaml</code> in the repository,
		else the jobs inferred from the repository's files.
	</p>
	{#if error && !dirty}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	{#if notice}
		<div class="alert alert-info" role="status">{notice}</div>
	{/if}
	{#if loading}
		<p class="muted">Loading…</p>
	{:else}
		<div class="tabs view-tabs" role="tablist" aria-label="Editor">
			<button
				class="tab"
				type="button"
				role="tab"
				id="index-config-tab-jobs"
				aria-selected={view === 'jobs'}
				aria-controls="index-config-panel"
				onclick={() => (view = 'jobs')}>Jobs</button
			>
			<button
				class="tab"
				type="button"
				role="tab"
				id="index-config-tab-json"
				aria-selected={view === 'json'}
				aria-controls="index-config-panel"
				onclick={() => (view = 'json')}>Edit as JSON</button
			>
		</div>
		<div
			id="index-config-panel"
			role="tabpanel"
			aria-labelledby={view === 'jobs' ? 'index-config-tab-jobs' : 'index-config-tab-json'}
		>
			{#if view === 'json'}
				<label class="label" for="index-config">Configuration (JSON)</label>
				<textarea
					id="index-config"
					class="input mono editor"
					rows="16"
					spellcheck="false"
					placeholder={'{"index_jobs": []}'}
					readonly={!siteAdmin}
					bind:value={text}
				></textarea>
			{:else if doc === null}
				<div class="alert alert-warning" role="status">
					The JSON has a syntax error, so the jobs can't be shown.
					<button class="btn btn-sm" type="button" onclick={() => (view = 'json')}>Fix it in the JSON editor</button>
				</div>
			{:else}
				<ConfigField
					field={jobsField}
					{doc}
					saved={savedDoc}
					issues={showRequired ? missing : []}
					disabled={!siteAdmin}
					onchange={setValue}
				/>
				{#if runsNoJobs}
					<div class="alert alert-warning" role="status" data-testid="index-config-no-jobs">
						A saved configuration without jobs indexes nothing, even if the repository has <code>sourcegull.yaml</code>
						or inferred jobs. Remove the configuration to use those again.
					</div>
				{/if}
			{/if}
		</div>
		{#if siteAdmin}
			<div class="actions">
				<button
					class="btn"
					disabled={busy || !inferred}
					onclick={() => (text = inferred?.configuration ?? '')}
					title="Replace the jobs with the inferred ones">Use inferred configuration</button
				>
				<button class="btn" disabled={busy} onclick={queue}>Queue jobs at HEAD</button>
				{#if saved !== null}
					<button
						class="btn"
						disabled={busy}
						onclick={remove}
						title="Delete the saved configuration, so sourcegull.yaml or inference applies">Remove configuration</button
					>
				{/if}
			</div>
		{/if}

		{#if siteAdmin}
			<SaveBar
				{dirty}
				{saving}
				note="A saved configuration takes precedence over sourcegull.yaml and inference."
				{savedAt}
				ondiscard={discard}
				onsave={save}
			>
				{#if showRequired && missing.length > 0}
					<div class="alert alert-danger" role="alert">
						<strong>Not saved.</strong> Fill in the required values marked in the form.
					</div>
				{:else if error}
					<div class="alert alert-danger" role="alert"><strong>Not saved.</strong> {error}</div>
				{/if}
			</SaveBar>
		{/if}

		<h3>Inferred configuration</h3>
		{#if inferred?.limitError}
			<div class="alert alert-danger" role="alert">{inferred.limitError}</div>
		{/if}
		{#if inferred?.parsedConfiguration?.length}
			<ul class="inferred" aria-label="Inferred jobs">
				{#each inferred.parsedConfiguration as j, i (i)}
					<li><code>{displayRoot(j.root)}</code> · {j.indexer?.name ?? 'unknown indexer'}</li>
				{/each}
			</ul>
			<details>
				<summary>JSON</summary>
				<pre class="json">{inferred.configuration}</pre>
			</details>
		{:else}
			<p class="muted">The recognizers find no projects to index at the default branch's head.</p>
		{/if}
	{/if}
</div>

<style>
	.intro {
		margin-top: 0;
	}
	.view-tabs {
		margin-bottom: var(--space-2);
	}
	.editor {
		width: 100%;
		font-family: var(--font-mono, monospace);
		font-size: 0.85em;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin: var(--space-2) 0 var(--space-4);
	}
	.inferred {
		margin: 0 0 var(--space-2);
	}
	.json {
		font-size: 0.8em;
		white-space: pre-wrap;
	}
</style>
