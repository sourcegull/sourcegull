<!--
	Create a search context in the viewer's namespace, one of their organizations', or (site
	admins) the instance's, unless the resources file owns every instance-level context.
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { DECLARED_KINDS, isExclusive, type DeclaredKindsResult } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import ContextForm from '#lib/search/ContextForm.svelte';
	import type { ContextFormValue } from '#lib/search/context-form.ts';
	import { createContext } from '#lib/search/contexts.ts';
	import { VIEWER_NAMESPACES, type ViewerNamespacesResult } from '#lib/search/queries.ts';

	let namespaces = $state.raw<{ id: string | null; label: string }[]>([]);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let ready = $state(false);
	/** Whether the resources file owns every instance-level context (site admins only). */
	let exclusive = $state(false);

	onMount(async () => {
		try {
			const r = await graphql<ViewerNamespacesResult>(VIEWER_NAMESPACES);
			const u = r.currentUser;
			if (u?.siteAdmin) {
				exclusive = await graphql<DeclaredKindsResult>(DECLARED_KINDS)
					.then((d) => isExclusive(d.site.declaredConfigFiles, 'searchContexts'))
					.catch(() => false);
			}
			if (u) {
				namespaces = [
					{ id: u.id, label: `@${u.username}` },
					...u.organizations.nodes.map((o) => ({ id: o.id, label: `@${o.name}` })),
					...(u.siteAdmin && !exclusive ? [{ id: null, label: 'Instance (no owner)' }] : [])
				];
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			ready = true;
		}
	});

	async function submit(v: ContextFormValue): Promise<void> {
		busy = true;
		error = null;
		try {
			await createContext(v);
			await goto('/contexts');
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>New search context · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<p class="crumbs"><a href="/contexts">Search contexts</a> /</p>
	<div class="page-header">
		<div>
			<h1>New search context</h1>
			<p class="page-description">A named set of repositories, or a query, that searches can be limited to.</p>
		</div>
	</div>
	{#if exclusive}<ManagedNotice kind="searchContexts" />{/if}
	{#if ready}
		<ContextForm
			initial={{
				namespace: namespaces[0]?.id ?? null,
				name: '',
				description: '',
				public: false,
				kind: 'query',
				query: '',
				repositories: ''
			}}
			{namespaces}
			submitLabel="Create context"
			{error}
			{busy}
			onsubmit={submit}
			oncancel={() => goto('/contexts')}
		/>
	{:else}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{/if}
</div>

