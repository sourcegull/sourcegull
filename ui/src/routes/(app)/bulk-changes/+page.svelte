<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { logEvent } from '#lib/events.ts';
	import BatchChangeList from '#lib/batches/BatchChangeList.svelte';

	// A usage event for the admin analytics.
	$effect(() => {
		logEvent('BatchChangeListPageViewed');
	});
</script>

<svelte:head><title>Bulk changes · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Bulk changes</h1>
			<p class="page-description">Run code changes across many repositories and track their merge requests.</p>
		</div>
		<div class="page-actions">
			<a class="btn btn-primary" href="/bulk-changes/create"><Icon name="plus" size={16} />Create bulk change</a>
		</div>
	</div>
	<BatchChangeList />
</div>
