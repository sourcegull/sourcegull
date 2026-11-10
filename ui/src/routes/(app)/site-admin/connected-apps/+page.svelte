<script lang="ts">
	import { onMount } from 'svelte';
	import Pager from '#lib/components/Pager.svelte';
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import SortHeader from '#lib/components/SortHeader.svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { OAuthGrantOrderBy } from '#lib/graphql-types.ts';
	import {
		clientLabel,
		CONNECTED_APPS,
		REVOKE_OAUTH_GRANT,
		type ConnectedAppsResult,
		type ConnectedAppsVariables,
		type OAuthGrantNode,
		type RevokeOAuthGrantVariables
	} from '#lib/oauth.ts';

	const pages = $state(cursorPages(50));
	let grants = $state<OAuthGrantNode[]>([]);
	let totalCount = $state(0);
	let query = $state('');
	let order = $state<OAuthGrantOrderBy>('CREATED_AT');
	let descending = $state(true);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let requestSeq = 0;

	async function load(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		error = null;
		try {
			const result = await graphql<ConnectedAppsResult, ConnectedAppsVariables>(CONNECTED_APPS, {
				first: pages.size,
				after: pageAfter(pages),
				query: query.trim() || null,
				orderBy: order,
				descending
			});
			if (seq !== requestSeq) return;
			grants = result.connectedApps.nodes;
			totalCount = result.connectedApps.totalCount;
			pageLoaded(pages, result.connectedApps.pageInfo);
			// The page emptied (apps revoked): step back to one with apps.
			if (grants.length === 0 && hasPreviousPage(pages)) {
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

	function sortBy(o: OAuthGrantOrderBy): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			// Names A to Z, dates newest first.
			descending = o === 'CREATED_AT' || o === 'LAST_USED_AT';
		}
		restart();
	}

	async function revoke(grant: OAuthGrantNode): Promise<void> {
		if (!confirm(`Revoke access of "${clientLabel(grant)}" for ${grant.user.username}? It will have to ask again.`)) {
			return;
		}
		pending = grant.id;
		error = null;
		try {
			await graphql<unknown, RevokeOAuthGrantVariables>(REVOKE_OAUTH_GRANT, { id: grant.id });
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			pending = null;
		}
	}

	const formatDate = (iso: string) => new Date(iso).toLocaleString();

	onMount(() => {
		void load();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Connected apps · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Connected apps</h1>
		<p class="page-description">
			MCP clients users allowed to act as them through OAuth sign-in. Revoking ends an app's access at once; the user
			can allow it again. Turn OAuth sign-in for MCP clients off with
			<code class="code-inline">mcp.oauth.enabled</code> in the site configuration.
		</p>
		<RelatedLinks />
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="list-bar">
	<label class="visually-hidden" for="app-filter">Filter connected apps</label>
	<input
		id="app-filter"
		class="input filter"
		type="search"
		placeholder="Filter by username or app…"
		bind:value={query}
		{oninput}
	/>
	<Pager {pages} shown={grants.length} total={totalCount} {loading} onchange={load} />
</div>

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<SortHeader key="APP" label="App" {order} {descending} onsort={sortBy} />
				<SortHeader key="USERNAME" label="User" {order} {descending} onsort={sortBy} />
				<th scope="col">Redirects to</th>
				<SortHeader key="CREATED_AT" label="Allowed" {order} {descending} onsort={sortBy} />
				<SortHeader key="LAST_USED_AT" label="Last used" {order} {descending} onsort={sortBy} />
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each grants as grant (grant.id)}
				<tr>
					<td>
						{clientLabel(grant)}
						{#if grant.clientURI}
							<a class="muted home" href={grant.clientURI} rel="noopener noreferrer" target="_blank">home page</a>
						{/if}
					</td>
					<td>
						<a href="/site-admin/users?query={encodeURIComponent(grant.user.username)}">{grant.user.username}</a>
					</td>
					<td>
						{#each grant.redirectHosts as host (host)}<code class="host">{host}</code>{/each}
					</td>
					<td>{formatDate(grant.createdAt)}</td>
					<td>{grant.lastUsedAt ? formatDate(grant.lastUsedAt) : 'Never'}</td>
					<td class="actions">
						<button
							class="btn btn-sm btn-danger"
							type="button"
							disabled={pending === grant.id}
							onclick={() => revoke(grant)}>Revoke</button
						>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="6" class="muted">
						{#if loading}Loading…{:else if query.trim()}No connected apps match.{:else}No user has connected an app.{/if}
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
	.home {
		margin-left: var(--space-2);
		font-size: var(--text-sm);
	}
	.host {
		margin-right: var(--space-1);
	}
	.actions {
		text-align: right;
	}
</style>
