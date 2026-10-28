<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_ACCESS_TOKEN,
		CURRENT_USER_ACCESS_TOKENS,
		DELETE_ACCESS_TOKEN,
		SCOPE_SITE_ADMIN_SUDO,
		SCOPE_USER_ALL,
		type AccessTokenNode,
		type CreateAccessTokenResult,
		type CreateAccessTokenVariables,
		type CurrentUserAccessTokensResult,
		type DeleteAccessTokenVariables
	} from '#lib/queries.ts';

	let user = $state<CurrentUserAccessTokensResult['currentUser']>(null);
	let tokens = $state<AccessTokenNode[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	let creating = $state(false);
	let note = $state('');
	let sudo = $state(false);
	let submitting = $state(false);
	/** The secret of a token just created. Shown once; never stored. */
	let newToken = $state<{ id: string; token: string } | null>(null);
	let copied = $state(false);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			const data = await graphql<CurrentUserAccessTokensResult>(CURRENT_USER_ACCESS_TOKENS);
			user = data.currentUser;
			tokens = data.currentUser?.accessTokens.nodes ?? [];
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function create(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!user) return;
		submitting = true;
		error = null;
		try {
			const scopes = sudo ? [SCOPE_USER_ALL, SCOPE_SITE_ADMIN_SUDO] : [SCOPE_USER_ALL];
			const data = await graphql<CreateAccessTokenResult, CreateAccessTokenVariables>(
				CREATE_ACCESS_TOKEN,
				{ user: user.id, scopes, note: note.trim() }
			);
			newToken = data.createAccessToken;
			copied = false;
			creating = false;
			note = '';
			sudo = false;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function remove(token: AccessTokenNode): Promise<void> {
		if (!confirm(`Delete the access token "${token.note}"? Anything using it will stop working.`)) return;
		error = null;
		try {
			await graphql<unknown, DeleteAccessTokenVariables>(DELETE_ACCESS_TOKEN, { byID: token.id });
			if (newToken?.id === token.id) newToken = null;
			tokens = tokens.filter((t) => t.id !== token.id);
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function copy(): Promise<void> {
		if (!newToken) return;
		try {
			await navigator.clipboard.writeText(newToken.token);
			copied = true;
		} catch {
			copied = false;
		}
	}

	const formatDate = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : '—');

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Access tokens · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Access tokens</h1>
		<p class="page-description">
			Let tools and scripts use the Sourcegull API as you. Send them as
			<code class="code-inline">Authorization: token &lt;token&gt;</code>.
		</p>
	</div>
	{#if !creating}
		<div class="page-actions">
			<button class="btn btn-primary" type="button" onclick={() => (creating = true)} disabled={!user}>
				Generate new token
			</button>
		</div>
	{/if}
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if newToken}
	<div class="alert alert-success" role="status">
		<strong>Copy your new access token now. You won't be able to see it again.</strong>
		<div class="new-token">
			<input
				class="input mono"
				readonly
				value={newToken.token}
				aria-label="New access token"
				onfocus={(e) => e.currentTarget.select()}
			/>
			<button class="btn" type="button" onclick={copy}>{copied ? 'Copied' : 'Copy'}</button>
		</div>
		<button class="btn-link btn" type="button" onclick={() => (newToken = null)}>Dismiss</button>
	</div>
{/if}

{#if creating}
	<form class="card create" onsubmit={create}>
		<h2>New access token</h2>
		<div class="field">
			<label for="token-note">Note</label>
			<!-- svelte-ignore a11y_autofocus -->
			<input
				id="token-note"
				class="input"
				bind:value={note}
				required
				placeholder="What's this token for?"
				autofocus
			/>
		</div>
		<fieldset class="field">
			<legend class="label">Scopes</legend>
			<label class="checkbox"><input type="checkbox" checked disabled /> <code>{SCOPE_USER_ALL}</code> — full
				access as you</label
			>
			{#if user?.siteAdmin}
				<label class="checkbox"
					><input type="checkbox" bind:checked={sudo} /> <code>{SCOPE_SITE_ADMIN_SUDO}</code> — act as
					any user (site admins only)</label
				>
			{/if}
		</fieldset>
		<div class="toolbar">
			<button class="btn btn-primary" type="submit" disabled={submitting || note.trim() === ''}>
				{submitting ? 'Generating…' : 'Generate token'}
			</button>
			<button class="btn" type="button" onclick={() => (creating = false)}>Cancel</button>
		</div>
	</form>
{/if}

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Note</th>
				<th scope="col">Scopes</th>
				<th scope="col">Created</th>
				<th scope="col">Last used</th>
				<th scope="col">Expires</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each tokens as token (token.id)}
				<tr>
					<td>{token.note}</td>
					<td>
						{#each token.scopes as scope (scope)}<code class="scope">{scope}</code>{/each}
					</td>
					<td>{formatDate(token.createdAt)}</td>
					<td>{token.lastUsedAt ? formatDate(token.lastUsedAt) : 'Never'}</td>
					<td>{token.expiresAt ? formatDate(token.expiresAt) : 'Never'}</td>
					<td class="actions">
						<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(token)}>Delete</button>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="6" class="muted">{loading ? 'Loading…' : 'You have no access tokens.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.new-token {
		display: flex;
		gap: var(--space-2);
		margin: var(--space-2) 0;
	}
	.create {
		margin-bottom: var(--space-6);
		max-width: 36rem;
	}
	fieldset {
		border: none;
		padding: 0;
		margin: 0 0 var(--space-4);
	}
	.scope {
		margin-right: var(--space-1);
	}
	.actions {
		text-align: right;
	}
</style>
