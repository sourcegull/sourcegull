<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ALL_OAUTH_GRANTS,
		clientLabel,
		CURRENT_USER_OAUTH_GRANTS,
		REVOKE_OAUTH_GRANT,
		type AllOAuthGrantsResult,
		type CurrentUserOAuthGrantsResult,
		type OAuthGrantNode,
		type RevokeOAuthGrantVariables
	} from '#lib/oauth.ts';

	/** Every user's grants (site admins) instead of the viewer's. */
	let { all = false }: { all?: boolean } = $props();

	let grants = $state<OAuthGrantNode[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			if (all) {
				grants = (await graphql<AllOAuthGrantsResult>(ALL_OAUTH_GRANTS)).oauthGrants;
			} else {
				const data = await graphql<CurrentUserOAuthGrantsResult>(CURRENT_USER_OAUTH_GRANTS);
				grants = data.currentUser?.oauthGrants ?? [];
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function revoke(grant: OAuthGrantNode): Promise<void> {
		const who = all ? ` for ${grant.user.username}` : '';
		if (!confirm(`Revoke access of "${clientLabel(grant)}"${who}? It will have to ask again.`)) return;
		error = null;
		try {
			await graphql<unknown, RevokeOAuthGrantVariables>(REVOKE_OAUTH_GRANT, { id: grant.id });
			grants = grants.filter((g) => g.id !== grant.id);
		} catch (err) {
			error = errorMessage(err);
		}
	}

	const formatDate = (iso: string) => new Date(iso).toLocaleString();
	const columns = $derived(all ? 6 : 5);

	onMount(() => {
		void load();
	});
</script>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">App</th>
				{#if all}<th scope="col">User</th>{/if}
				<th scope="col">Redirects to</th>
				<th scope="col">Allowed</th>
				<th scope="col">Last used</th>
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
					{#if all}<td>{grant.user.username}</td>{/if}
					<td>
						{#each grant.redirectHosts as host (host)}<code class="host">{host}</code>{/each}
					</td>
					<td>{formatDate(grant.createdAt)}</td>
					<td>{grant.lastUsedAt ? formatDate(grant.lastUsedAt) : 'Never'}</td>
					<td class="actions">
						<button class="btn btn-sm btn-danger" type="button" onclick={() => revoke(grant)}>Revoke</button>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan={columns} class="muted">
						{loading ? 'Loading…' : all ? 'No user has connected an app.' : 'You have not connected any apps.'}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
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
