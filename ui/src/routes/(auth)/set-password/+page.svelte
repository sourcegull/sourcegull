<script lang="ts">
	import { page } from '$app/state';
	import { setInvitePassword } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';

	const token = $derived(page.url.searchParams.get('token') ?? '');

	let password = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);
	let done = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await setInvitePassword(token, password);
			done = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Set your password · Sourcegull</title></svelte:head>

<h1>Set your password</h1>

{#if done}
	<div class="alert alert-success" role="status">
		Your password is set. <a href="/sign-in">Sign in</a>.
	</div>
{:else if !token}
	<div class="alert alert-danger" role="alert">
		This link is incomplete. Use the full link a site admin sent you.
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
