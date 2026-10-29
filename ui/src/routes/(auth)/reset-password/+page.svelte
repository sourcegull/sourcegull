<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { resetAuthState, resetPassword } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';

	const userID = $derived(page.url.searchParams.get('userID') ?? '');
	const code = $derived(page.url.searchParams.get('code') ?? '');

	let password = $state('');
	let confirmPassword = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		error = null;
		if (password !== confirmPassword) {
			error = 'The passwords do not match.';
			return;
		}
		submitting = true;
		try {
			await resetPassword(userID, code, password);
			// The server ended every session, this browser's too: sign in again with the new password.
			resetAuthState();
			await goto('/sign-in?local=1&reset=1', { replaceState: true, invalidateAll: true });
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Set a new password · Sourcegull</title></svelte:head>

<h1>Set a new password</h1>

{#if !userID || !code}
	<div class="alert alert-danger" role="alert">
		This link is incomplete. Use the full link from the email, or request a new one.
	</div>
{:else}
	<p class="muted">Setting it signs you out everywhere; then sign in with the new password.</p>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	<form {onsubmit}>
		<div class="field">
			<label class="label" for="password">New password</label>
			<input
				class="input"
				id="password"
				type="password"
				bind:value={password}
				autocomplete="new-password"
				required
			/>
		</div>
		<div class="field">
			<label class="label" for="confirm-password">Confirm new password</label>
			<input
				class="input"
				id="confirm-password"
				type="password"
				bind:value={confirmPassword}
				autocomplete="new-password"
				required
			/>
		</div>
		<button class="btn btn-primary" type="submit" disabled={submitting}>
			{submitting ? 'Saving…' : 'Set password'}
		</button>
	</form>
{/if}
