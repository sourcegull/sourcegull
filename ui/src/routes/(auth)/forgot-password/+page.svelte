<script lang="ts">
	import { requestPasswordReset } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	let email = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);
	let sent = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await requestPasswordReset(email);
			sent = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Reset your password · Sourcegull</title></svelte:head>

<h1>Reset your password</h1>

{#if !data.auth.emailEnabled}
	<div class="alert alert-danger" role="alert">
		Email is not set up on this instance, so reset links cannot be emailed. Ask a site admin to
		create a reset link for you (Site admin &gt; Users &gt; Reset password), or to set up email.
	</div>
{:else if sent}
	<div class="alert alert-success" role="status">
		If an account has this verified email, a reset link is on its way.
	</div>
{:else}
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	<form {onsubmit}>
		<div class="field">
			<label class="label" for="email">Email</label>
			<input
				class="input"
				id="email"
				type="email"
				bind:value={email}
				required
				autocomplete="email"
			/>
		</div>
		<button class="btn btn-primary" type="submit" disabled={submitting}>
			{submitting ? 'Sending…' : 'Send reset link'}
		</button>
	</form>
{/if}

<p class="muted footer"><a href="/sign-in?local=1">Back to sign in</a></p>

<style>
	.footer {
		margin: var(--space-4) 0 0;
		text-align: center;
	}
</style>
