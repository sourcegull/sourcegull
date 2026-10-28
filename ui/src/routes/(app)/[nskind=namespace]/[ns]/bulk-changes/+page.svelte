<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { logEvent } from '#lib/events.ts';
	import { page } from '$app/state';
	import BatchChangeList from '#lib/batches/BatchChangeList.svelte';
	import type { NamespaceKind } from '#lib/batches/batch-change.ts';

	const namespace = $derived({
		kind: (page.params.nskind ?? 'users') as NamespaceKind,
		name: page.params.ns ?? ''
	});

	// A usage event for the admin analytics.
	$effect(() => {
		logEvent('BatchChangeListPageViewed');
	});
</script>

<svelte:head><title>Bulk changes of {namespace.name} · Sourcegull</title></svelte:head>

<div class="page">
	<p class="crumbs"><a href="/bulk-changes">Bulk changes</a> /</p>
	<div class="page-header">
		<div>
			<h1 class="ns">{namespace.name}</h1>
			<p class="page-description">Bulk changes owned by {namespace.name}.</p>
		</div>
		<div class="page-actions">
			<a class="btn btn-primary" href="/bulk-changes/create"><Icon name="plus" size={16} />Create bulk change</a>
		</div>
	</div>
	<BatchChangeList {namespace} />
</div>

<style>
	.ns {
		overflow-wrap: anywhere;
	}
</style>
