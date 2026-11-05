<!-- The `notices` setting at one location. Messages are Markdown, rendered without any HTML. -->
<script lang="ts">
	import Markdown from '#lib/batches/Markdown.svelte';
	import { loadDismissedNotices, notices, storeDismissedNotices, type NoticeLocation } from './viewer.ts';
	import { viewerSettings } from './viewer-settings.ts';

	let { location }: { location: NoticeLocation } = $props();

	let dismissed = $state(loadDismissedNotices());
	const shown = $derived(
		notices($viewerSettings, location).filter((n) => !(n.dismissible && dismissed.includes(n.message)))
	);

	function dismiss(message: string): void {
		dismissed = [...dismissed, message];
		storeDismissedNotices(dismissed);
	}
</script>

{#each shown as notice, i (i)}
	<div class="alert alert-info notice" role="status" data-testid="notice-{location}">
		<div class="message"><Markdown source={notice.message} /></div>
		{#if notice.dismissible}
			<button class="btn btn-sm" type="button" onclick={() => dismiss(notice.message)}>Dismiss</button>
		{/if}
	</div>
{/each}

<style>
	.notice {
		display: flex;
		align-items: flex-start;
		gap: var(--space-3);
	}
	.message {
		flex: 1;
		min-width: 0;
	}
</style>
