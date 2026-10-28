<script lang="ts">
	import type {
		CodeIntelligenceInferenceScriptQuery,
		InferAutoIndexJobsForRepoQuery,
		InferenceRepositoryIdQuery
	} from '#lib/graphql-types.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		EXAMPLE_SCRIPT,
		INFER_AUTO_INDEX_JOBS,
		INFERENCE_SCRIPT,
		REPOSITORY_ID,
		UPDATE_INFERENCE_SCRIPT,
		type InferredJob
	} from '#lib/codeintel/inference.ts';
	import { displayRoot } from '#lib/repo/codegraph.ts';

	let saved = $state('');
	let script = $state('');
	let loading = $state(true);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let repoName = $state('');
	let jobs = $state.raw<InferredJob[] | null>(null);
	let output = $state('');

	$effect(() => {
		graphql<CodeIntelligenceInferenceScriptQuery>(INFERENCE_SCRIPT)
			.then((r) => {
				saved = r.codeIntelligenceInferenceScript;
				script = saved;
			})
			.catch((err) => (error = errorMessage(err)))
			.finally(() => (loading = false));
	});

	async function save(): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			await graphql(UPDATE_INFERENCE_SCRIPT, { script });
			saved = script;
			notice = script.trim() ? 'Saved the inference script.' : 'Removed the inference script: the built-in recognizers run unchanged.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function tryIt(): Promise<void> {
		busy = true;
		error = null;
		jobs = null;
		try {
			const r = await graphql<InferenceRepositoryIdQuery, { name: string }>(REPOSITORY_ID, { name: repoName.trim() });
			if (!r.repository) {
				error = `No repository named ${repoName.trim()}.`;
				return;
			}
			const res = await graphql<
				InferAutoIndexJobsForRepoQuery,
				{ repo: string; script: string }
			>(
				INFER_AUTO_INDEX_JOBS,
				{ repo: r.repository.id, script }
			);
			jobs = res.inferAutoIndexJobsForRepo.jobs;
			output = res.inferAutoIndexJobsForRepo.inferenceOutput;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Inference script · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Auto-indexing inference script</h1>
		<p class="page-description">
			A Lua script that adds recognizers to auto-indexing's job inference or disables built-in ones. It
			runs sandboxed (no I/O, limited time and memory). Auto-indexing itself is turned on with
			<code class="code-inline">codeIntelAutoIndexing.enabled</code> in the
			<a href="/site-admin/configuration">site configuration</a>.
		</p>
	</div>
</div>
{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if notice}
	<div class="alert alert-info" role="status">{notice}</div>
{/if}
{#if loading}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{:else}
	<section class="card">
		<div class="card-header">
			<h2><label for="inference-script">Script</label></h2>
		</div>
		<textarea
			id="inference-script"
			class="input editor"
			rows="22"
			spellcheck="false"
			placeholder="-- No script: the built-in recognizers run unchanged."
			bind:value={script}
		></textarea>
		<div class="actions">
			<button class="btn btn-primary" disabled={busy || script === saved} onclick={save}>Save</button>
			{#if !script.trim()}
				<button class="btn" onclick={() => (script = EXAMPLE_SCRIPT)}>Insert an example</button>
			{/if}
		</div>
	</section>

	<section class="card try-card">
		<div class="card-header">
			<h2>Try it</h2>
			<span class="muted small">Runs the script above (saved or not) against a repository</span>
		</div>
		<form
			class="try"
			onsubmit={(e) => {
				e.preventDefault();
				void tryIt();
			}}
		>
			<label class="visually-hidden" for="try-repo">Repository</label>
			<input id="try-repo" class="input" placeholder="Repository name" bind:value={repoName} />
			<button class="btn" type="submit" disabled={busy || !repoName.trim()}>Infer jobs</button>
		</form>
		{#if jobs}
			{#if jobs.length === 0}
				<div class="empty-state"><p>No jobs inferred.</p></div>
			{:else}
				<div class="table-scroll">
					<table class="table table-bordered table-sm" aria-label="Inferred jobs">
						<thead><tr><th>Root</th><th>Indexer</th><th>Command</th></tr></thead>
						<tbody>
							{#each jobs as j, i (i)}
								<tr>
									<td class="mono">{displayRoot(j.root)}</td>
									<td title={j.indexer?.imageName ?? ''}>{j.indexer?.name ?? ''}</td>
									<td class="mono">{j.steps.index.indexerArgs.join(' ')}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
			{#if output}
				<h3>Output</h3>
				<pre class="output">{output}</pre>
			{/if}
		{/if}
	</section>
{/if}

<style>
	.card-header label {
		font: inherit;
	}
	.editor {
		width: 100%;
		font-family: var(--font-mono);
		font-size: var(--text-sm);
	}
	.actions,
	.try {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin: var(--space-3) 0 0;
	}
	.try {
		margin: 0 0 var(--space-4);
	}
	.try input {
		flex: 1 1 16rem;
		max-width: 32rem;
	}
	.try-card {
		margin-top: var(--space-4);
	}
	.output {
		white-space: pre-wrap;
		font-family: var(--font-mono);
		font-size: var(--text-sm);
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-3);
	}
</style>
