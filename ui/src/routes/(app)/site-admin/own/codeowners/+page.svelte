<script lang="ts">
	import { formatDateTime } from '#lib/format.svelte.ts';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import Pager from '#lib/components/Pager.svelte';
	import SortHeader from '#lib/components/SortHeader.svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { CodeownersFilesQueryVariables, CodeownersOrderBy } from '#lib/graphql-types.ts';
	import {
		ADD_CODEOWNERS_FILE,
		CODEOWNERS_FILES,
		DELETE_CODEOWNERS_FILES,
		UPDATE_CODEOWNERS_FILE,
		type CodeownersFile,
		type CodeownersFilesResult
	} from '#lib/own/queries.ts';

	// Ingested CODEOWNERS files (Code Ownership): a file uploaded here replaces the
	// repository's own CODEOWNERS file for ownership and `file:has.owner()`.

	type Vars = Record<string, unknown>;

	const pages = $state(cursorPages(50));
	let files = $state<CodeownersFile[]>([]);
	let totalCount = $state(0);
	let query = $state('');
	let order = $state<CodeownersOrderBy>('REPOSITORY');
	let descending = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let saving = $state(false);
	/** The repository being edited (null: a new file). */
	let editing = $state<string | null>(null);
	let repoName = $state(page.url.searchParams.get('repo') ?? '');
	let contents = $state('');
	let requestSeq = 0;

	const fetchFiles = (vars: CodeownersFilesQueryVariables) =>
		graphql<CodeownersFilesResult, CodeownersFilesQueryVariables>(CODEOWNERS_FILES, vars);

	async function load(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		error = null;
		try {
			const r = await fetchFiles({
				first: pages.size,
				after: pageAfter(pages),
				query: query.trim() || null,
				orderBy: order,
				descending
			});
			if (seq !== requestSeq) return;
			files = r.codeownersIngestedFiles.nodes;
			totalCount = r.codeownersIngestedFiles.totalCount;
			pageLoaded(pages, r.codeownersIngestedFiles.pageInfo);
			// The page emptied (files deleted): step back to one with files.
			if (files.length === 0 && hasPreviousPage(pages)) {
				previousPage(pages);
				void load();
				return;
			}
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	/** Opens the file of `?repo=` for editing when there is one (links from a repository). */
	async function editWanted(): Promise<void> {
		const wanted = page.url.searchParams.get('repo');
		if (!wanted) return;
		try {
			const r = await fetchFiles({ first: 50, query: wanted });
			const f = r.codeownersIngestedFiles.nodes.find((x) => x.repository?.name === wanted);
			if (f) edit(f);
		} catch (err) {
			error = errorMessage(err);
		}
	}

	function restart(): void {
		firstPage(pages);
		void load();
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(restart, 200);
	}

	function sortBy(o: CodeownersOrderBy): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			// Repositories A to Z, the latest upload first.
			descending = o === 'UPDATED_AT';
		}
		restart();
	}

	function edit(f: CodeownersFile): void {
		editing = f.repository?.name ?? null;
		repoName = f.repository?.name ?? '';
		contents = f.contents;
	}

	function reset(): void {
		editing = null;
		repoName = '';
		contents = '';
	}

	async function save(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		saving = true;
		error = null;
		try {
			const input = { repoName: repoName.trim(), fileContents: contents };
			await graphql<unknown, Vars>(editing ? UPDATE_CODEOWNERS_FILE : ADD_CODEOWNERS_FILE, { input });
			reset();
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	async function remove(f: CodeownersFile): Promise<void> {
		if (!f.repository) return;
		if (!confirm(`Delete the ingested CODEOWNERS file of ${f.repository.name}?`)) return;
		error = null;
		try {
			await graphql<unknown, Vars>(DELETE_CODEOWNERS_FILES, {
				repositories: [{ repoID: f.repository.id }]
			});
			if (editing === f.repository.name) reset();
			await load();
		} catch (err) {
			error = errorMessage(err);
		}
	}

	const formatDate = (iso: string) => formatDateTime(iso);

	onMount(() => {
		void load();
		void editWanted();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Code ownership · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>CODEOWNERS files</h1>
		<p class="page-description">
			A CODEOWNERS file uploaded for a repository replaces the one in the repository (GitLab syntax: sections, <span
				class="code-inline">@user</span
			>, <span class="code-inline">@group/subgroup</span>, emails). Owners resolve to users through their GitLab
			accounts and verified emails, and groups to teams of the same name.
		</p>
	</div>
	<div class="page-actions">
		<span class="badge">{totalCount} ingested</span>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<form class="card editor" onsubmit={save}>
	<div class="card-header">
		<h2>{editing ? `Edit ${editing}` : 'Upload a CODEOWNERS file'}</h2>
	</div>
	<label class="field">
		<span class="label">Repository</span>
		<input
			class="input mono"
			type="text"
			placeholder="gitlab.example.com/group/project"
			bind:value={repoName}
			disabled={!!editing || saving}
			required
		/>
	</label>
	<label class="field">
		<span class="label">Contents</span>
		<textarea
			class="input mono"
			rows="12"
			bind:value={contents}
			disabled={saving}
			required
			placeholder={'* @default-owner\n[Docs]\n/docs/ @docs-team'}
		></textarea>
	</label>
	<div class="actions">
		<button class="btn btn-primary" type="submit" disabled={saving || !repoName.trim()}>
			{editing ? 'Save' : 'Upload'}
		</button>
		{#if editing}<button class="btn" type="button" onclick={reset} disabled={saving}>Cancel</button>{/if}
	</div>
</form>

<div class="list-bar">
	<label class="visually-hidden" for="codeowners-filter">Filter by repository</label>
	<input
		id="codeowners-filter"
		class="input filter"
		type="search"
		placeholder="Filter by repository…"
		bind:value={query}
		{oninput}
	/>
	<Pager {pages} shown={files.length} total={totalCount} {loading} onchange={load} />
</div>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<SortHeader key="REPOSITORY" label="Repository" {order} {descending} onsort={sortBy} />
				<th scope="col" class="num">Lines</th>
				<SortHeader key="UPDATED_AT" label="Updated" {order} {descending} onsort={sortBy} />
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each files as f (f.id)}
				<tr>
					<td class="mono">
						{#if f.repository}<a href={f.repository.url}>{f.repository.name}</a>{:else}<span class="muted"
								>(not visible)</span
							>{/if}
					</td>
					<td class="num">{f.contents.split('\n').filter((l) => l.trim()).length}</td>
					<td>{formatDate(f.updatedAt)}</td>
					<td class="row-actions">
						<button class="btn btn-sm" type="button" onclick={() => edit(f)}>Edit</button>
						<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(f)}>Delete</button>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="4" class="empty-cell">
						{#if loading}Loading…{:else if query.trim()}No repository with an uploaded file matches.{:else}No CODEOWNERS
							files have been uploaded.{/if}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.list-bar {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
	}
	.list-bar .filter {
		flex: 0 1 20rem;
		width: auto;
		min-width: 12rem;
	}
	.editor {
		margin: 0 0 var(--space-6);
		max-width: 48rem;
	}
	.editor textarea {
		width: 100%;
		resize: vertical;
	}
	.actions {
		display: flex;
		gap: var(--space-2);
	}
	.row-actions {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-1);
		text-align: right;
		white-space: nowrap;
	}
</style>
