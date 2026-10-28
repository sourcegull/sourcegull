<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import ContextForm from '#lib/search/ContextForm.svelte';
	import { formatRepositoryLines, type ContextFormValue } from '#lib/search/context-form.ts';
	import { updateContext } from '#lib/search/contexts.ts';
	import {
		SEARCH_CONTEXT_BY_SPEC,
		namespaceLabel,
		type SearchContextBySpecResult,
		type SearchContextDetail
	} from '#lib/search/queries.ts';

	const spec = $derived(page.url.searchParams.get('spec') ?? '');

	let context = $state.raw<SearchContextDetail | null>(null);
	let loadError = $state<string | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);

	$effect(() => {
		const s = spec;
		context = null;
		loadError = null;
		graphql<SearchContextBySpecResult, { spec: string }>(SEARCH_CONTEXT_BY_SPEC, { spec: s })
			.then((r) => {
				context = r.searchContextBySpec;
				if (!context) loadError = `No search context ${s}.`;
				else if (!context.viewerCanManage && !context.managedBy) loadError = `You cannot edit ${s}.`;
			})
			.catch((err) => (loadError = errorMessage(err)));
	});

	function initial(c: SearchContextDetail): ContextFormValue {
		return {
			namespace: c.namespace?.id ?? null,
			name: c.name,
			description: c.description,
			public: c.public,
			kind: c.repositories.length > 0 ? 'repositories' : 'query',
			query: c.query,
			repositories: formatRepositoryLines(
				c.repositories.map((r) => ({ name: r.repository.name, revisions: r.revisions }))
			)
		};
	}

	async function submit(v: ContextFormValue): Promise<void> {
		if (!context) return;
		busy = true;
		error = null;
		try {
			await updateContext(context.id, v);
			await goto('/contexts');
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Edit {spec} · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<p class="crumbs"><a href="/contexts">Search contexts</a> /</p>
	<div class="page-header">
		<div>
			<h1 class="mono">{spec}</h1>
			{#if context}<p class="page-description">Owned by {namespaceLabel(context.namespace)}.</p>{/if}
		</div>
	</div>
	{#if loadError}
		<div class="alert alert-danger" role="alert">{loadError}</div>
	{:else if context?.managedBy}
		<ManagedNotice managedBy={context.managedBy} />
		<a class="btn" href="/contexts">Back to search contexts</a>
	{:else if context}
		{#key context.id}
			<ContextForm
				initial={initial(context)}
				submitLabel="Save changes"
				{error}
				{busy}
				onsubmit={submit}
				oncancel={() => goto('/contexts')}
			/>
		{/key}
	{:else}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{/if}
</div>

