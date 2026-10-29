<script lang="ts">
	import { requestAccess } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';

	let name = $state('');
	let email = $state('');
	let additionalInfo = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);
	let sent = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await requestAccess(name, email, additionalInfo);
			sent = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Request access · Sourcegull</title></svelte:head>

<h1>Request access</h1>

{#if sent}
	<div class="alert alert-success" role="status">
		Thank you. A site admin will review your request and get in touch.
	</div>
{:else}
	<p class="muted">Sign-up is closed on this instance. Ask a site admin for an account.</p>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	<form {onsubmit}>
		<div class="field">
			<label class="label" for="name">Name</label>
			<input
				class="input"
				id="name"
				bind:value={name}
				required
				maxlength="255"
				autocomplete="name"
			/>
		</div>
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
		<div class="field">
			<label class="label" for="info"
				>Why do you need access? <span class="muted">(optional)</span></label
			>
			<textarea class="input" id="info" rows="4" bind:value={additionalInfo} maxlength="4096"
			></textarea>
		</div>
		<button class="btn btn-primary" type="submit" disabled={submitting}>
			{submitting ? 'Sending…' : 'Request access'}
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
