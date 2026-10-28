<script lang="ts">
	import { goto } from '$app/navigation';
	import { resetAuthState, signUp } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	const siteInit = $derived(data.auth.needsSiteInit);

	let username = $state('');
	let email = $state('');
	let password = $state('');
	let error = $state<string | null>(null);
	let submitting = $state(false);

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		const wasSiteInit = siteInit;
		try {
			await signUp(username, email, password);
			resetAuthState();
			// The first user is the site admin; take them straight to the configuration.
			await goto(wasSiteInit ? '/site-admin/configuration' : '/search', { replaceState: true });
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>{siteInit ? 'Set up Sourcegull' : 'Sign up'} · Sourcegull</title></svelte:head>

{#if siteInit}
	<h1>Welcome!</h1>
	<p class="muted">
		Create the first account to finish setting up this instance. It will be the site admin.
	</p>
{:else}
	<h1>Create an account</h1>
{/if}

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<form {onsubmit}>
	<div class="field">
		<label for="username">Username</label>
		<!-- svelte-ignore a11y_autofocus -->
		<input
			id="username"
			class="input"
			bind:value={username}
			autocomplete="username"
			autocapitalize="off"
			spellcheck="false"
			required
			autofocus
		/>
	</div>
	<div class="field">
		<label for="email">Email</label>
		<input id="email" class="input" type="email" bind:value={email} autocomplete="email" required />
	</div>
	<div class="field">
		<label for="password">Password</label>
		<input
			id="password"
			class="input"
			type="password"
			bind:value={password}
			autocomplete="new-password"
			required
		/>
	</div>
	<button class="btn btn-primary full" type="submit" disabled={submitting}>
		{#if submitting}Creating account…{:else if siteInit}Create admin account{:else}Sign up{/if}
	</button>
</form>

{#if !siteInit}
	<p class="muted footer">Already have an account? <a href="/sign-in">Sign in</a></p>
{/if}

<style>
	.full {
		width: 100%;
		justify-content: center;
	}
	.footer {
		margin: var(--space-4) 0 0;
		text-align: center;
	}
</style>
