<script lang="ts">
	import { page } from '$app/state';
	import RepoPage from '#lib/repo/RepoPage.svelte';
	import { parseRepoRoute } from '#lib/repo/url.ts';

	// Repository pages have no fixed prefix (names contain slashes), so every path that no other
	// route claims lands here and is parsed as `/{repo}[@{rev}][/-/{kind}/{path}]`.
	const route = $derived(parseRepoRoute(page.url.pathname));
</script>

<svelte:head>{#if !route}<title>Not found · Sourcegull</title>{/if}</svelte:head>

{#if route}
	<RepoPage {route} />
{:else}
	<div class="not-found">
		<h1>Page not found</h1>
		<p class="muted">There is nothing at <code>{page.url.pathname}</code>.</p>
		<p><a href="/repositories">Browse repositories</a> or <a href="/search">search</a>.</p>
	</div>
{/if}

<style>
	.not-found {
		max-width: 40rem;
		margin: 10vh auto;
		padding: 0 var(--space-4);
	}
</style>
