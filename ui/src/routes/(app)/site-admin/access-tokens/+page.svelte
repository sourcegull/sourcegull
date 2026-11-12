<script lang="ts">
	import { formatDate } from '#lib/format.svelte.ts';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import CountTile from '#lib/components/CountTile.svelte';
	import Pager from '#lib/components/Pager.svelte';
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import SortHeader from '#lib/components/SortHeader.svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { AccessTokenOrderBy, AccessTokenState } from '#lib/graphql-types.ts';
	import {
		DELETE_ACCESS_TOKEN,
		SITE_ACCESS_TOKENS,
		type DeleteAccessTokenVariables,
		type SiteAccessTokensResult,
		type SiteAccessTokensVariables
	} from '#lib/queries.ts';

	type Token = SiteAccessTokensResult['accessTokens']['nodes'][number];

	const pages = $state(cursorPages(50));
	let tokens = $state<Token[]>([]);
	let totalCount = $state(0);
	let counts = $state<SiteAccessTokensResult['accessTokenCounts'] | null>(null);
	// `?query=` prefills the filter, for links from a user.
	let query = $state(page.url.searchParams.get('query') ?? '');
	let show = $state<AccessTokenState>('ALL');
	let order = $state<AccessTokenOrderBy>('CREATED_AT');
	let descending = $state(true);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let requestSeq = 0;

	async function load(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		error = null;
		try {
			const result = await graphql<SiteAccessTokensResult, SiteAccessTokensVariables>(SITE_ACCESS_TOKENS, {
				first: pages.size,
				after: pageAfter(pages),
				query: query.trim() || null,
				state: show,
				orderBy: order,
				descending
			});
			if (seq !== requestSeq) return;
			tokens = result.accessTokens.nodes;
			totalCount = result.accessTokens.totalCount;
			counts = result.accessTokenCounts;
			pageLoaded(pages, result.accessTokens.pageInfo);
			// The page emptied (tokens revoked): step back to one with tokens.
			if (tokens.length === 0 && hasPreviousPage(pages)) {
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

	function sortBy(o: AccessTokenOrderBy): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			// Names A to Z, dates newest first; for expiry, the soonest first.
			descending = o === 'CREATED_AT' || o === 'LAST_USED_AT';
		}
		restart();
	}

	/** Shows only expired or unused tokens; a second click shows every token again. */
	function showOnly(state: AccessTokenState): void {
		show = show === state ? 'ALL' : state;
		restart();
	}

	function showAll(): void {
		clearTimeout(debounce);
		query = '';
		show = 'ALL';
		restart();
	}

	const tokenLabel = (t: Token) => `skp_${t.prefix}…`;

	async function revoke(t: Token): Promise<void> {
		const what = t.note ? `"${t.note}" (${tokenLabel(t)})` : tokenLabel(t);
		if (!confirm(`Revoke ${t.subject.username}'s token ${what}? It stops working at once.`)) return;
		pending = t.id;
		error = null;
		notice = null;
		try {
			await graphql<unknown, DeleteAccessTokenVariables>(DELETE_ACCESS_TOKEN, { byID: t.id });
			notice = `Revoked ${t.subject.username}'s token ${what}.`;
			await load();
		} catch (err) {
			error = `Could not revoke ${tokenLabel(t)}: ${errorMessage(err)}`;
		} finally {
			pending = null;
		}
	}

	const expired = (t: Token) => t.expiresAt !== null && new Date(t.expiresAt).getTime() <= Date.now();

	onMount(() => {
		void load();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Access tokens · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Access tokens</h1>
		<p class="page-description">
			Every user's access tokens for the API and MCP. Revoking a token stops it working at once. Who may create tokens
			and how long they last is set by <code class="code-inline">auth.accessTokens</code> in the site configuration.
		</p>
		<RelatedLinks />
	</div>
</div>

<div class="tiles" role="group" aria-label="Show tokens">
	<CountTile
		value={counts?.total ?? null}
		label="tokens"
		active={show === 'ALL' && !query.trim()}
		title="Show every token and clear the filter"
		onclick={showAll}
	/>
	<CountTile
		value={counts?.expired ?? null}
		label="expired"
		tone={counts?.expired ? 'warning' : undefined}
		active={show === 'EXPIRED'}
		title="Show only expired tokens"
		disabled={!counts?.expired && show !== 'EXPIRED'}
		onclick={() => showOnly('EXPIRED')}
	/>
	<CountTile
		value={counts?.unused ?? null}
		label="unused 90+ days"
		tone={counts?.unused ? 'warning' : undefined}
		active={show === 'UNUSED'}
		title="Show only unexpired tokens not used in the last 90 days"
		disabled={!counts?.unused && show !== 'UNUSED'}
		onclick={() => showOnly('UNUSED')}
	/>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if notice}
	<div class="alert alert-success" role="status">{notice}</div>
{/if}

<div class="list-bar">
	<label class="visually-hidden" for="token-filter">Filter tokens</label>
	<input
		id="token-filter"
		class="input filter"
		type="search"
		placeholder="Filter by username or note…"
		bind:value={query}
		{oninput}
	/>
	{#if show !== 'ALL'}
		<span class="shown">
			<strong>Only {show === 'EXPIRED' ? 'expired tokens' : 'tokens unused for 90+ days'}</strong>
			<button type="button" class="btn btn-link" onclick={showAll}>Show all tokens</button>
		</span>
	{/if}
	<Pager {pages} shown={tokens.length} total={totalCount} {loading} onchange={load} />
</div>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<SortHeader key="USERNAME" label="User" {order} {descending} onsort={sortBy} />
				<th scope="col">Token</th>
				<th scope="col">Scopes</th>
				<SortHeader key="CREATED_AT" label="Created" {order} {descending} onsort={sortBy} />
				<SortHeader key="LAST_USED_AT" label="Last used" {order} {descending} onsort={sortBy} />
				<SortHeader key="EXPIRES_AT" label="Expires" {order} {descending} onsort={sortBy} />
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each tokens as t (t.id)}
				<tr>
					<td>
						<a href="/site-admin/users?query={encodeURIComponent(t.subject.username)}">{t.subject.username}</a>
						{#if t.creator.id !== t.subject.id}<div class="muted small">created by {t.creator.username}</div>{/if}
					</td>
					<td>
						<code class="mono">{tokenLabel(t)}</code>
						{#if t.note}<div class="note">{t.note}</div>{/if}
					</td>
					<td>
						{#each t.scopes as scope (scope)}<span class="badge" class:badge-warning={scope !== 'user:all'}
								>{scope}</span
							>{/each}
					</td>
					<td>{formatDate(t.createdAt)}</td>
					<td>{t.lastUsedAt ? formatDate(t.lastUsedAt) : 'Never'}</td>
					<td>
						{#if t.expiresAt}
							{formatDate(t.expiresAt)}
							{#if expired(t)}<span class="badge badge-warning">Expired</span>{/if}
						{:else}
							Never
						{/if}
					</td>
					<td class="actions">
						<button class="btn btn-sm btn-danger" type="button" disabled={pending === t.id} onclick={() => revoke(t)}
							>Revoke</button
						>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="7" class="empty-cell">
						{#if loading}Loading…{:else if query.trim() || show !== 'ALL'}No tokens match.{:else}No user has an access
							token.{/if}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(8rem, 10rem));
		gap: var(--space-2);
		margin-bottom: var(--space-5);
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
	.shown {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.note {
		margin-top: var(--space-1);
	}
	td :global(.badge + .badge) {
		margin-left: var(--space-1);
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
</style>
