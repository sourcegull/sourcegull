<script lang="ts">
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import {
		answerAuthorization,
		authorizeURL,
		clientLabel,
		loadAuthorization,
		type AuthorizationRequest
	} from '#lib/oauth.ts';

	const request = page.url.searchParams.get('request');
	// An app's request that needs the user signed in first comes as `?authorize=`; the layout
	// has signed the user in, so it goes back to /.oauth/authorize.
	const resume = authorizeURL(page.url.searchParams.get('authorize'));
	// Errors of /.oauth/authorize that cannot go back to the app come as `?error=`.
	let error = $state<string | null>(page.url.searchParams.get('error'));
	let details = $state<AuthorizationRequest | null>(null);
	let loading = $state(request !== null || resume !== null);
	let submitting = $state(false);

	onMount(async () => {
		if (request === null && resume !== null) {
			window.location.replace(resume);
			return;
		}
		if (request === null) {
			error ??= 'This page is opened by an app asking for access to Sourcegull. Start again from the app.';
			return;
		}
		try {
			details = await loadAuthorization(request);
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		} finally {
			loading = false;
		}
	});

	async function answer(approve: boolean): Promise<void> {
		if (request === null) return;
		submitting = true;
		error = null;
		try {
			window.location.assign(await answerAuthorization(request, approve));
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
			submitting = false;
		}
	}
</script>

<svelte:head><title>Authorize app · Sourcegull</title></svelte:head>

{#if details}
	<h1>Allow {clientLabel(details)} to use Sourcegull as you?</h1>
{:else}
	<h1>Authorize app</h1>
{/if}

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if loading}
	<p class="muted">Loading…</p>
{:else if details}
	<p>
		<strong>{clientLabel(details)}</strong> wants to search and read code in Sourcegull as
		<strong>{details.username}</strong>, through the MCP server.
	</p>
	{#if details.redirectWarning}
		<div class="alert alert-warning" role="alert">
			This app registered itself, so anyone could have chosen its name. It sends you to
			<code class="code-inline">{details.redirectHost}</code>, which is not on your computer. Only allow
			it if you know and trust that site.
		</div>
	{/if}
	<ul class="facts">
		<li>It sees exactly the repositories you can, and cannot change anything.</li>
		<li>
			After you allow it, you are sent back to <code class="code-inline">{details.redirectHost}</code>.
			{#if details.metadataDocument}
				The app is described at <code class="code-inline">{details.clientID}</code>.
			{:else}
				The app registered itself; only allow it if you just started connecting an app.
			{/if}
		</li>
		{#if details.clientURI}
			<li>App home page: <a href={details.clientURI} rel="noopener noreferrer" target="_blank">{details.clientURI}</a></li>
		{/if}
		<li>You can revoke access any time under <a href="/user/settings/connected-apps">Connected apps</a>.</li>
	</ul>
	<div class="actions">
		<button class="btn btn-primary" type="button" onclick={() => answer(true)} disabled={submitting}>
			{submitting ? 'Redirecting…' : 'Allow'}
		</button>
		<button class="btn" type="button" onclick={() => answer(false)} disabled={submitting}>Deny</button>
	</div>
{:else}
	<p class="muted footer"><a href="/search">Go to Sourcegull</a></p>
{/if}

<style>
	.facts {
		margin: var(--space-4) 0;
		padding-left: var(--space-5);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		font-size: var(--text-sm);
	}
	.facts code {
		overflow-wrap: anywhere;
	}
	.actions {
		display: flex;
		gap: var(--space-2);
	}
	.actions .btn {
		flex: 1;
		justify-content: center;
	}
	.footer {
		text-align: center;
	}
</style>
