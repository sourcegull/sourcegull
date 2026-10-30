<script lang="ts">
	import { page } from '$app/state';
	import { relatedForRoute, type RelatedLink } from '#lib/admin/related.ts';

	/** The links to show; defaults to those of the current site admin route. */
	let { links }: { links?: RelatedLink[] } = $props();
	// Pages that share one list (such as the code intelligence pages) leave out themselves.
	const shown = $derived(
		(links ?? relatedForRoute(page.route.id)).filter((l) => l.href !== page.url.pathname)
	);
</script>

{#if shown.length > 0}
	<nav class="related small" aria-label="Related pages">
		<span class="muted">Related:</span>
		<ul role="list">
			{#each shown as link (link.href)}
				<li><a href={link.href}>{link.title}</a></li>
			{/each}
		</ul>
	</nav>
{/if}

<style>
	.related {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: var(--space-1) var(--space-2);
		margin-top: var(--space-2);
	}
	ul {
		display: contents;
		list-style: none;
	}
	li + li::before {
		content: '·';
		margin-right: var(--space-2);
		color: var(--color-text-muted);
	}
</style>
