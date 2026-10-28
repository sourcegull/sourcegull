<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { ACCOUNT_PASSWORD, UPDATE_PASSWORD, type AccountPasswordResult } from '#lib/account/queries.ts';

	let hasPassword = $state<boolean | null>(null);
	let oldPassword = $state('');
	let newPassword = $state('');
	let confirmPassword = $state('');
	let error = $state<string | null>(null);
	let saved = $state(false);
	let busy = $state(false);

	async function load(): Promise<void> {
		try {
			const r = await graphql<AccountPasswordResult>(ACCOUNT_PASSWORD);
			hasPassword = r.currentUser?.hasPassword ?? false;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function save(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		error = null;
		saved = false;
		if (newPassword !== confirmPassword) {
			error = 'The new passwords do not match.';
			return;
		}
		busy = true;
		try {
			await graphql<unknown, Record<string, unknown>>(UPDATE_PASSWORD, {
				oldPassword: hasPassword ? oldPassword : null,
				newPassword
			});
			oldPassword = newPassword = confirmPassword = '';
			hasPassword = true;
			saved = true;
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

<svelte:head><title>Password · Settings · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Password</h1>
		<p class="page-description">
			Changing your password signs you out everywhere else; this browser stays signed in.
		</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if saved}<div class="alert alert-success" role="status">Password changed.</div>{/if}

{#if hasPassword !== null}
	<form class="form" onsubmit={save}>
		<input type="text" name="username" autocomplete="username" hidden />
		{#if hasPassword}
			<div class="field">
				<label for="old-password">Current password</label>
				<input id="old-password" class="input" type="password" bind:value={oldPassword} autocomplete="current-password" required />
			</div>
		{/if}
		<div class="field">
			<label for="new-password">New password</label>
			<input id="new-password" class="input" type="password" bind:value={newPassword} autocomplete="new-password" required />
		</div>
		<div class="field">
			<label for="confirm-password">Confirm new password</label>
			<input id="confirm-password" class="input" type="password" bind:value={confirmPassword} autocomplete="new-password" required />
		</div>
		<button class="btn btn-primary" type="submit" disabled={busy}>
			{hasPassword ? 'Change password' : 'Set password'}
		</button>
	</form>
{:else if !error}
	<p class="muted">Loading…</p>
{/if}

<style>
	.form {
		max-width: 24rem;
		display: grid;
		gap: var(--space-4);
		justify-items: start;
	}
	.field {
		width: 100%;
	}
</style>
