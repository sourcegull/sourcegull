<!--
	"Targeted repositories" of the search and capture group creation pages: a repository
	query (re-evaluated at every recording, checked by the server, with the number of repositories
	it matches today), an explicit list, or all repositories.
-->
<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import RepositoriesInput from './RepositoriesInput.svelte';
	import { repoFieldsError, type RepoMode } from './creation.ts';
	import {
		INSIGHT_REPOSITORIES_COUNT,
		VALIDATE_INSIGHT_REPO_QUERY,
		type InsightRepositoriesCountResult,
		type ValidateInsightRepoQueryResult
	} from './queries.ts';

	interface Props {
		repoMode: RepoMode;
		repoQuery: string;
		repositories: string[];
		/** Show errors of untouched fields (after a submit). */
		submitted: boolean;
		/** The server's verdict on the repository query (null: valid or not checked). */
		queryError: string | null;
		/** Whether the server check of the query is running. */
		checking: boolean;
	}

	let {
		repoMode = $bindable(),
		repoQuery = $bindable(),
		repositories = $bindable(),
		submitted,
		queryError = $bindable(),
		checking = $bindable()
	}: Props = $props();

	let touched = $state(false);
	let count = $state<number | null>(null);

	const localError = $derived(repoFieldsError({ repoMode, repoQuery, repositories }));
	const error = $derived(localError ?? (repoMode === 'search-query' ? queryError : null));
	const showError = $derived((touched || submitted) && error !== null);

	// The server checks the query and counts its repositories, debounced.
	$effect(() => {
		const q = repoQuery.trim();
		if (repoMode !== 'search-query' || q === '') {
			queryError = null;
			checking = false;
			count = null;
			return;
		}
		checking = true;
		let cancelled = false;
		const timer = setTimeout(async () => {
			try {
				const v = await graphql<ValidateInsightRepoQueryResult, { query: string }>(VALIDATE_INSIGHT_REPO_QUERY, {
					query: q
				});
				if (cancelled) return;
				queryError = v.validateScopedInsightQuery.isValid
					? null
					: (v.validateScopedInsightQuery.invalidReason ?? 'The repositories query is not valid.');
				if (queryError === null) {
					const c = await graphql<InsightRepositoriesCountResult, { query: string }>(INSIGHT_REPOSITORIES_COUNT, {
						query: q
					});
					if (!cancelled) count = c.previewRepositoriesFromQuery.numberOfRepositories ?? 0;
				} else {
					count = null;
				}
			} catch (err) {
				if (!cancelled) queryError = errorMessage(err);
			} finally {
				if (!cancelled) checking = false;
			}
		}, 500);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
	});
</script>

<fieldset class="section" aria-describedby="insight-repositories-error">
	<legend><h2>Targeted repositories</h2></legend>

	<label class="radio">
		<input type="radio" name="repoMode" value="search-query" bind:group={repoMode} />
		<span>Repositories query</span>
		{#if repoMode === 'search-query'}
			<span class="muted count" role="status">Repositories count: {count ?? 0}</span>
		{/if}
	</label>
	{#if repoMode === 'search-query'}
		<div class="mode-body">
			<input
				id="insight-repo-query"
				class="input mono"
				aria-label="Repositories query"
				placeholder="Example: repo:gitlab.com/acme/*"
				aria-invalid={showError ? 'true' : undefined}
				aria-busy={checking}
				bind:value={repoQuery}
				onblur={() => (touched = true)}
			/>
			<ul class="hint muted">
				<li>Hint: you can use regular expressions within each of the <code>repo:</code> filters</li>
				<li>
					Data points will be automatically backfilled using the list of repositories resulting from today’s search.
					Future data points will use the list refreshed for every snapshot.
				</li>
			</ul>
		</div>
	{/if}

	<label class="radio">
		<input type="radio" name="repoMode" value="urls-list" bind:group={repoMode} />
		<span>Explicit list of repositories</span>
	</label>
	{#if repoMode === 'urls-list'}
		<div class="mode-body">
			<RepositoriesInput
				id="insight-repositories"
				bind:value={repositories}
				invalid={showError}
				describedby="insight-repositories-hint"
				onblur={() => (touched = true)}
			/>
			<span class="hint muted" id="insight-repositories-hint">Find and choose at least 1 repository to run trend</span>
		</div>
	{/if}

	<label class="radio">
		<input type="radio" name="repoMode" value="all-repos" bind:group={repoMode} />
		<span>All repositories</span>
	</label>
	{#if repoMode === 'all-repos'}
		<p class="mode-body hint muted">
			The trend runs over every repository, including those added later. Its live preview counts a limited number of
			repositories.
		</p>
	{/if}

	{#if showError}<span class="error" id="insight-repositories-error" role="alert">{error}</span>{/if}
</fieldset>

<style>
	.section {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
	}
	legend {
		padding: 0;
		margin-bottom: var(--space-2);
	}
	h2 {
		margin: 0;
		font-size: 1.05rem;
	}
	.radio {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		font-weight: 600;
	}
	.count {
		margin-left: auto;
		font-weight: 400;
		font-size: 0.85rem;
	}
	.mode-body {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		margin: 0 0 var(--space-2) 1.5rem;
	}
	ul.hint {
		margin: 0;
		padding-left: 1.2rem;
		font-size: 0.85rem;
	}
	.hint {
		font-size: 0.85rem;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
</style>
