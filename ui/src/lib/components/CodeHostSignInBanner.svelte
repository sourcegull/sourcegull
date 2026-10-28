<!--
	Asks the signed-in user to sign in again with a code host whose token the permissions sync
	cannot use, so their private repositories show up. Loaded once per page load; failures are
	silent (the banner is a hint, not a requirement).
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { graphql } from '#lib/graphql.ts';
	import type { CodeHostSignInsNeededQuery } from '#lib/graphql-types.ts';
	import { CODE_HOST_SIGN_INS_NEEDED, signInPrompts, type CodeHostSignIn } from '#lib/account/code-host-sign-in.ts';

	let needed = $state<CodeHostSignIn[]>([]);
	const prompts = $derived(signInPrompts(needed, page.url.pathname + page.url.search));

	onMount(() => {
		graphql<CodeHostSignInsNeededQuery>(CODE_HOST_SIGN_INS_NEEDED)
			.then((r) => (needed = r.currentUser?.codeHostSignInsNeeded ?? []))
			.catch(() => {});
	});
</script>

{#each prompts as p (p.key)}
	<div class="alert alert-warning banner" role="status">
		{p.message}
		{#if p.href}<a href={p.href}>{p.linkText}</a>{/if}
	</div>
{/each}

<style>
	.banner {
		margin: var(--space-3) var(--space-4) 0;
	}
	.banner a {
		margin-left: var(--space-2);
		font-weight: var(--weight-medium);
	}
</style>
