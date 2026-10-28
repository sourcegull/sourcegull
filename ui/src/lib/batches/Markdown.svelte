<!-- Renders Markdown parsed by `markdown.ts` with text nodes only (no HTML from the source). -->
<script lang="ts">
	import { parseMarkdown, type Inline } from './markdown.ts';

	let { source }: { source: string } = $props();

	const blocks = $derived(parseMarkdown(source));
</script>

{#snippet inline(nodes: Inline[])}
	{#each nodes as n, i (i)}
		{#if n.kind === 'text'}{n.text}{:else if n.kind === 'code'}<code>{n.text}</code
			>{:else if n.kind === 'strong'}<strong>{@render inline(n.children)}</strong
			>{:else if n.kind === 'em'}<em>{@render inline(n.children)}</em
			>{:else if n.kind === 'link'}<a href={n.href} rel="noopener noreferrer">{@render inline(n.children)}</a
			>{/if}
	{/each}
{/snippet}

<div class="markdown">
	{#each blocks as b, i (i)}
		{#if b.kind === 'heading'}
			<p class="heading h{b.level}" role="heading" aria-level={b.level + 2}>{@render inline(b.children)}</p>
		{:else if b.kind === 'para'}
			<p>{@render inline(b.children)}</p>
		{:else if b.kind === 'code'}
			<pre><code>{b.text}</code></pre>
		{:else if b.kind === 'rule'}
			<hr />
		{:else if b.kind === 'quote'}
			<blockquote>{@render inline(b.children)}</blockquote>
		{:else if b.ordered}
			<ol>
				{#each b.items as item, j (j)}<li>{@render inline(item)}</li>{/each}
			</ol>
		{:else}
			<ul>
				{#each b.items as item, j (j)}<li>{@render inline(item)}</li>{/each}
			</ul>
		{/if}
	{/each}
</div>

<style>
	.markdown {
		overflow-wrap: anywhere;
	}
	.markdown > :first-child {
		margin-top: 0;
	}
	.markdown > :last-child {
		margin-bottom: 0;
	}
	p {
		margin: 0 0 var(--space-2);
		white-space: pre-line;
	}
	.heading {
		font-weight: 600;
		margin-top: var(--space-3);
	}
	.h1 {
		font-size: 1.25em;
	}
	.h2 {
		font-size: 1.1em;
	}
	pre {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-2) var(--space-3);
		overflow-x: auto;
	}
	blockquote {
		margin: 0 0 var(--space-2);
		padding-left: var(--space-3);
		border-left: 3px solid var(--color-border-strong);
		color: var(--color-text-muted);
		white-space: pre-line;
	}
	ul,
	ol {
		margin: 0 0 var(--space-2);
		padding-left: var(--space-6);
	}
</style>
