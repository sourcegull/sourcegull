<script lang="ts">
	import { page } from '$app/state';
	import { resetPassword } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';

	const userID = $derived(page.url.searchParams.get('userID') ?? '');
	const code = $derived(page.url.searchParams.get('code') ?? '');

	let password = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);
	let done = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await resetPassword(userID, code, password);
			done = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Set a new password · Sourcegull</title></svelte:head>

<h1>Set a new password</h1>

{#if done}
	<div class="alert alert-success" role="status">
		Your password is changed and your other sessions are signed out.
		<a href="/sign-in">Sign in</a>.
	</div>
{:else if !userID || !code}
	<div class="alert alert-danger" role="alert">
		This link is incomplete. Use the full link from the email, or request a new one.
	</div>
{:else}
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
		<button class="btn btn-primary" type="submit" disabled={submitting}>
			{submitting ? 'Saving…' : 'Set password'}
		</button>
	</form>
{/if}
