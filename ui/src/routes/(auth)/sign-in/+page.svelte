<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import {
		builtinFormCollapsed,
		providerLogo,
		providerSignInURL,
		resetAuthState,
		safeReturnTo,
		signIn
	} from '#lib/auth.ts';
	import GiteaLogo from '#lib/components/GiteaLogo.svelte';
	import GitHubLogo from '#lib/components/GitHubLogo.svelte';
	import GitLabLogo from '#lib/components/GitLabLogo.svelte';
	import Icon from '#lib/components/Icon.svelte';
	import { errorMessage } from '#lib/graphql.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	let username = $state('');
	let password = $state('');
	// External sign-in failures come back as `?error=`.
	let error = $state<string | null>(page.url.searchParams.get('error'));
	const returnTo = $derived(safeReturnTo(page.url.searchParams.get('returnTo')));
	const providers = $derived(data.auth.providers ?? []);
	const builtin = $derived(data.auth.builtin !== false);
	let submitting = $state(false);
	// With external providers the password form sits behind a toggle; a failed sign-in keeps it open.
	const collapsible = $derived(builtin && providers.length > 0);
	// Decided once on load; afterwards only the user (or a failed sign-in) changes it.
	let builtinOpen = $state(untrack(() => !builtinFormCollapsed(data.auth, page.url)));
	let usernameInput = $state<HTMLInputElement | null>(null);

	function ontoggle(event: Event) {
		if ((event.currentTarget as HTMLDetailsElement).open) usernameInput?.focus();
	}

	async function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await signIn(username, password);
			resetAuthState();
			await goto(returnTo, { replaceState: true });
		} catch (err) {
			error = errorMessage(err);
			builtinOpen = true;
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Sign in · Sourcegull</title></svelte:head>

<h1>Sign in</h1>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if providers.length > 0}
	<div class="providers">
		{#each providers as p (p.authenticationURL)}
			<a class="btn full" href={providerSignInURL(p, returnTo)} data-sveltekit-reload>
				{#if providerLogo(p) === 'gitlab'}<GitLabLogo />{:else if providerLogo(p) === 'github'}<GitHubLogo
					/>{:else if providerLogo(p) === 'gitea'}<GiteaLogo />{:else}<Icon name="key" />{/if}
				Sign in with {p.displayName}
			</a>
		{/each}
	</div>
{/if}

{#snippet builtinForm(autofocus: boolean)}
	<form {onsubmit}>
		<div class="field">
			<label for="username">Username or email</label>
			<!-- svelte-ignore a11y_autofocus -->
			<input
				id="username"
				class="input"
				bind:this={usernameInput}
				bind:value={username}
				autocomplete="username"
				required
				{autofocus}
			/>
		</div>
		<div class="field">
			<label for="password">Password</label>
			<input
				id="password"
				class="input"
				type="password"
				bind:value={password}
				autocomplete="current-password"
				required
			/>
		</div>
		<button class="btn btn-primary full" type="submit" disabled={submitting}>
			{submitting ? 'Signing in…' : 'Sign in'}
		</button>
	</form>
	<p class="muted footer"><a href="/forgot-password">Forgot password?</a></p>
{/snippet}

{#if collapsible}
	<details class="builtin" bind:open={builtinOpen} {ontoggle}>
		<summary class="muted">Sign in with username and password</summary>
		{@render builtinForm(false)}
	</details>
{:else if builtin}
	{@render builtinForm(true)}
{/if}

{#if data.auth.allowSignup}
	<p class="muted footer">New here? <a href="/sign-up">Create an account</a></p>
{:else if data.auth.accessRequest}
	<p class="muted footer">Need an account? <a href="/request-access">Request access</a></p>
{/if}

<style>
	.full {
		width: 100%;
		justify-content: center;
	}
	.providers {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.builtin {
		margin-top: var(--space-4);
	}
	.builtin > summary {
		width: fit-content;
		margin: 0 auto;
		cursor: pointer;
		font-size: var(--text-sm);
	}
	.builtin > summary:hover {
		color: var(--color-text);
	}
	.builtin[open] > summary {
		margin-bottom: var(--space-4);
	}
	.footer {
		margin: var(--space-4) 0 0;
		text-align: center;
	}
</style>
