<script lang="ts">
	import type { QueueAutoIndexJobsForRepoMutation } from '#lib/graphql-types.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		INDEX_CONFIGURATION,
		QUEUE_AUTO_INDEX_JOBS,
		UPDATE_INDEX_CONFIGURATION,
		displayRoot,
		type IndexConfigurationResult,
		type InferredConfiguration
	} from './codegraph.ts';

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
		busy = true;
		error = null;
		notice = null;
		try {
			await graphql(UPDATE_INDEX_CONFIGURATION, { repo: repoId, configuration: text });
			saved = text.trim() ? text : null;
			notice = saved ? 'Saved. It takes precedence over sourcegull.yaml and inference.' : 'Removed the configuration.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function queue(): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			const r = await graphql<
				QueueAutoIndexJobsForRepoMutation,
				{ repo: string; rev: string | null; configuration: string | null }
			>(
				QUEUE_AUTO_INDEX_JOBS,
				{ repo: repoId, rev: null, configuration: null }
			);
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
		Auto-indexing runs the jobs of this configuration if one is saved, else those of <code>sourcegull.yaml</code> in the
		repository, else the jobs inferred from the repository's files.
	</p>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	{#if notice}
		<div class="alert alert-info" role="status">{notice}</div>
	{/if}
	{#if loading}
		<p class="muted">Loading…</p>
	{:else}
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
		{#if siteAdmin}
			<div class="actions">
				<button class="btn btn-primary" disabled={busy || text === (saved ?? '')} onclick={save}>Save</button>
				<button
					class="btn"
					disabled={busy || !inferred}
					onclick={() => (text = inferred?.configuration ?? '')}
					title="Replace the text with the inferred configuration">Use inferred configuration</button
				>
				<button class="btn" disabled={busy} onclick={queue}>Queue jobs at HEAD</button>
			</div>
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
	.editor {
		width: 100%;
		font-family: var(--font-mono, monospace);
		font-size: 0.85em;
	}
	.actions {
		display: flex;
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
