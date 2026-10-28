<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type {
		AddRepoKeyValuePairMutationVariables,
		DeleteRepoKeyValuePairMutationVariables,
		UpdateRepoKeyValuePairMutationVariables
	} from '#lib/graphql-types.ts';
	import {
		ADD_REPO_KEY_VALUE_PAIR,
		DELETE_REPO_KEY_VALUE_PAIR,
		REPOSITORY_METADATA,
		UPDATE_REPO_KEY_VALUE_PAIR,
		canWriteMetadata,
		metadataFilter,
		type RepositoryMetadataResult
	} from './metadata.ts';

	interface Props {
		/** The repository's name. */
		repo: string;
	}

	let { repo }: Props = $props();

	type Pair = { key: string; value: string | null };

	let repoId = $state<string | null>(null);
	let pairs = $state.raw<Pair[]>([]);
	let canWrite = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state(false);

	let newKey = $state('');
	let newValue = $state('');
	/** The key being edited, and its value in the form. */
	let editing = $state<string | null>(null);
	let editValue = $state('');

	$effect(() => {
		void load(repo);
	});

	async function load(name: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<RepositoryMetadataResult, { name: string }>(REPOSITORY_METADATA, { name });
			if (name !== repo) return;
			repoId = r.repository?.id ?? null;
			pairs = r.repository?.keyValuePairs ?? [];
			canWrite = canWriteMetadata(r.currentUser);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	/** An empty value makes the key a tag. */
	const valueOf = (s: string) => (s.trim() === '' ? null : s.trim());

	async function run(f: () => Promise<unknown>): Promise<boolean> {
		busy = true;
		error = null;
		try {
			await f();
			await load(repo);
			return true;
		} catch (err) {
			error = errorMessage(err);
			return false;
		} finally {
			busy = false;
		}
	}

	async function add(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!repoId || newKey.trim() === '') return;
		const vars = { repo: repoId, key: newKey.trim(), value: valueOf(newValue) };
		if (await run(() => graphql<unknown, AddRepoKeyValuePairMutationVariables>(ADD_REPO_KEY_VALUE_PAIR, vars))) {
			newKey = '';
			newValue = '';
		}
	}

	async function save(event: SubmitEvent, key: string): Promise<void> {
		event.preventDefault();
		if (!repoId) return;
		const vars = { repo: repoId, key, value: valueOf(editValue) };
		if (await run(() => graphql<unknown, UpdateRepoKeyValuePairMutationVariables>(UPDATE_REPO_KEY_VALUE_PAIR, vars))) {
			editing = null;
		}
	}

	async function remove(key: string): Promise<void> {
		if (!repoId || !confirm(`Delete the key "${key}"?`)) return;
		const vars = { repo: repoId, key };
		await run(() => graphql<unknown, DeleteRepoKeyValuePairMutationVariables>(DELETE_REPO_KEY_VALUE_PAIR, vars));
	}

	function edit(p: Pair): void {
		editing = p.key;
		editValue = p.value ?? '';
	}

	const searchURL = (p: Pair) => {
		const f = metadataFilter(p.key, p.value);
		return f ? `/search?q=${encodeURIComponent(f)}` : null;
	};
</script>

<div class="settings">
	<h2>Metadata</h2>
	<p class="muted">
		Key-value pairs and tags (a key with no value) describe the repository. Search finds repositories by them with
		<code>repo:has.meta(key:value)</code> and <code>repo:has.tag(tag)</code>.
	</p>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}

	{#if !loading && !canWrite}
		<div class="alert alert-info">
			You need the <code>REPO_METADATA#WRITE</code> permission to change this repository's metadata.
		</div>
	{/if}

	<div class="table-scroll">
		<table class="table" aria-busy={loading}>
			<thead>
				<tr>
					<th scope="col">Key</th>
					<th scope="col">Value</th>
					{#if canWrite}<th scope="col"><span class="visually-hidden">Actions</span></th>{/if}
				</tr>
			</thead>
			<tbody>
				{#each pairs as p (p.key)}
					<tr>
						<td>
							{#if searchURL(p)}<a class="mono" href={searchURL(p)}>{p.key}</a>{:else}<span class="mono">{p.key}</span>{/if}
						</td>
						<td>
							{#if editing === p.key}
								<form class="inline" onsubmit={(e) => save(e, p.key)}>
									<!-- svelte-ignore a11y_autofocus -->
									<input
										class="input input-sm"
										bind:value={editValue}
										aria-label="Value of {p.key}"
										placeholder="No value (tag)"
										autofocus
									/>
									<button class="btn btn-sm btn-primary" type="submit" disabled={busy}>Save</button>
									<button class="btn btn-sm" type="button" onclick={() => (editing = null)}>Cancel</button>
								</form>
							{:else if p.value === null}
								<span class="muted">tag</span>
							{:else}
								<span class="mono">{p.value}</span>
							{/if}
						</td>
						{#if canWrite}
							<td class="actions">
								{#if editing !== p.key}
									<button class="btn btn-sm" type="button" onclick={() => edit(p)} disabled={busy}>Edit</button>
									<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(p.key)} disabled={busy}>
										Delete
									</button>
								{/if}
							</td>
						{/if}
					</tr>
				{:else}
					<tr>
						<td colspan={canWrite ? 3 : 2} class="muted">{loading ? 'Loading…' : 'No metadata.'}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>

	{#if canWrite}
		<form class="add" onsubmit={add}>
			<div class="field">
				<label for="kvp-key">Key</label>
				<input id="kvp-key" class="input" bind:value={newKey} required />
			</div>
			<div class="field">
				<label for="kvp-value">Value</label>
				<input id="kvp-value" class="input" bind:value={newValue} placeholder="Empty for a tag" />
			</div>
			<button class="btn btn-primary" type="submit" disabled={busy || newKey.trim() === ''}>Add</button>
		</form>
	{/if}
</div>

<style>
	.settings {
		max-width: 48rem;
		padding: var(--space-4);
	}
	.inline {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	.add {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
		align-items: flex-end;
		margin-top: var(--space-4);
	}
	.add .field {
		margin: 0;
	}
</style>
