<script lang="ts">
	import Pager from '#lib/components/Pager.svelte';
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import SortHeader from '#lib/components/SortHeader.svelte';
	import { onMount } from 'svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { OrgOrderBy } from '#lib/graphql-types.ts';
	import {
		CREATE_ORGANIZATION,
		DELETE_ORGANIZATION,
		ORGANIZATIONS,
		type CreateOrganizationResult,
		type OrganizationsResult,
		type OrgSummary,
		type Vars
	} from '#lib/orgs/queries.ts';

	const pages = $state(cursorPages(50));
	let orgs = $state<OrgSummary[]>([]);
	let totalCount = $state(0);
	let query = $state('');
	let order = $state<OrgOrderBy>('NAME');
	let descending = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let name = $state('');
	let displayName = $state('');
	let requestSeq = 0;

	async function load(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		try {
			const result = await graphql<OrganizationsResult, Vars>(ORGANIZATIONS, {
				first: pages.size,
				after: pageAfter(pages),
				query: query.trim() || null,
				orderBy: order,
				descending
			});
			if (seq !== requestSeq) return;
			orgs = result.organizations.nodes;
			totalCount = result.organizations.totalCount;
			pageLoaded(pages, result.organizations.pageInfo);
			error = null;
			// The page emptied (organizations deleted): step back to one with organizations.
			if (orgs.length === 0 && hasPreviousPage(pages)) {
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

	function restart(): void {
		firstPage(pages);
		void load();
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(restart, 200);
	}

	function sortBy(o: OrgOrderBy): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			// Names A to Z, newest first.
			descending = o === 'CREATED_AT';
		}
		restart();
	}

	async function create(): Promise<void> {
		busy = 'create';
		error = null;
		try {
			await graphql<CreateOrganizationResult, Vars>(CREATE_ORGANIZATION, {
				name: name.trim(),
				displayName: displayName.trim() || null
			});
			name = '';
			displayName = '';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function remove(org: OrgSummary): Promise<void> {
		if (!confirm(`Delete the organization ${org.name}? Its members lose access to its namespace.`)) return;
		busy = org.id;
		error = null;
		try {
			await graphql(DELETE_ORGANIZATION, { organization: org.id });
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	onMount(() => {
		void load();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Organizations · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Organizations</h1>
		<p class="page-description">
			{totalCount} total. Organizations share settings, saved searches and bulk changes among their members.
		</p>
		<RelatedLinks />
	</div>
</div>

<section class="card create-card">
	<div class="card-header"><h2>New organization</h2></div>
	<form
		class="toolbar create-row"
		onsubmit={(event) => {
			event.preventDefault();
			void create();
		}}
	>
		<label class="visually-hidden" for="org-name">Name</label>
		<input id="org-name" class="input" placeholder="Name (e.g. acme)" bind:value={name} required />
		<label class="visually-hidden" for="org-display-name">Display name</label>
		<input id="org-display-name" class="input" placeholder="Display name (optional)" bind:value={displayName} />
		<button class="btn btn-primary" type="submit" disabled={busy !== null || !name.trim()}>Create organization</button>
	</form>
	<p class="muted small hint">
		You become the first member of an organization you create; invite others from its page.
	</p>
</section>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="list-bar">
	<label class="visually-hidden" for="org-filter">Filter organizations</label>
	<input
		id="org-filter"
		class="input filter"
		type="search"
		placeholder="Filter by name or display name…"
		bind:value={query}
		{oninput}
	/>
	<Pager {pages} shown={orgs.length} total={totalCount} {loading} onchange={load} />
</div>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<SortHeader key="NAME" label="Name" {order} {descending} onsort={sortBy} />
				<th scope="col">Display name</th>
				<SortHeader key="CREATED_AT" label="Created" {order} {descending} onsort={sortBy} />
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each orgs as org (org.id)}
				<tr>
					<td><a href={org.url}><strong>{org.name}</strong></a></td>
					<td>{org.displayName ?? ''}</td>
					<td>{new Date(org.createdAt).toLocaleDateString()}</td>
					<td class="actions">
						<a class="btn btn-sm" href="{org.url}/members">Members</a>
						<a class="btn btn-sm" href="{org.url}/settings">Settings</a>
						<button class="btn btn-sm btn-danger" type="button" disabled={busy !== null} onclick={() => remove(org)}
							>Delete</button
						>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="4" class="empty-cell">
						{#if loading}Loading…{:else if query.trim()}No organizations match.{:else}No organizations.{/if}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.create-card {
		max-width: 48rem;
		margin-bottom: var(--space-6);
	}
	.create-row .input {
		flex: 1 1 12rem;
		width: auto;
	}
	.hint {
		margin: var(--space-2) 0 0;
	}
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
	.actions {
		text-align: right;
		white-space: nowrap;
	}
</style>
