<script lang="ts">
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { resetAuthState } from '#lib/auth.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ACCOUNT_PROFILE,
		UPDATE_PROFILE,
		type AccountProfileResult,
		type UpdateProfileResult
	} from '#lib/account/queries.ts';

	let user = $state<AccountProfileResult['currentUser']>(null);
	let username = $state('');
	let displayName = $state('');
	let avatarURL = $state('');
	let error = $state<string | null>(null);
	let saved = $state(false);
	let busy = $state(false);

	async function load(): Promise<void> {
		try {
			const r = await graphql<AccountProfileResult>(ACCOUNT_PROFILE);
			user = r.currentUser;
			username = user?.username ?? '';
			displayName = user?.displayName ?? '';
			avatarURL = user?.avatarURL ?? '';
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function save(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!user) return;
		busy = true;
		error = null;
		saved = false;
		try {
			const renamed = user.viewerCanChangeUsername && username.trim() !== user.username;
			const r = await graphql<UpdateProfileResult, Record<string, unknown>>(UPDATE_PROFILE, {
				user: user.id,
				username: renamed ? username.trim() : null,
				displayName,
				avatarURL
			});
			user = { ...user, ...r.updateUser };
			saved = true;
			if (renamed) {
				resetAuthState();
				await invalidateAll();
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Profile · Settings · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Profile</h1>
		<p class="page-description">Your name and picture as other users see them.</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if saved}<div class="alert alert-success" role="status">Profile saved.</div>{/if}

{#if user}
	<form class="form" onsubmit={save}>
		<div class="field">
			<label for="profile-username">Username</label>
			<input
				id="profile-username"
				class="input"
				bind:value={username}
				disabled={!user.viewerCanChangeUsername}
				autocomplete="username"
				required
			/>
			{#if !user.viewerCanChangeUsername}
				<p class="hint muted">
					{user.scimControlled
						? 'Your identity provider manages your username.'
						: 'Ask a site admin to change your username.'}
				</p>
			{/if}
		</div>
		<div class="field">
			<label for="profile-display-name">Display name</label>
			<input id="profile-display-name" class="input" bind:value={displayName} maxlength="255" autocomplete="name" />
		</div>
		<div class="field">
			<label for="profile-avatar">Avatar URL</label>
			<input id="profile-avatar" class="input" type="url" bind:value={avatarURL} placeholder="https://…" />
			{#if avatarURL}<img class="avatar-preview" src={avatarURL} alt="" width="48" height="48" />{/if}
		</div>
		<button class="btn btn-primary" type="submit" disabled={busy}>Save profile</button>
	</form>
{:else if !error}
	<p class="muted">Loading…</p>
{/if}

<style>
	.form {
		max-width: 32rem;
		display: grid;
		gap: var(--space-4);
		justify-items: start;
	}
	.field {
		width: 100%;
	}
	.hint {
		margin: var(--space-1) 0 0;
	}
	.avatar-preview {
		margin-top: var(--space-2);
		border-radius: 50%;
		object-fit: cover;
	}
</style>
