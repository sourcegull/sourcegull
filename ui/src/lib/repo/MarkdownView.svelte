<!-- Renders a repository Markdown document (`markdown.ts`) with text nodes only: no HTML from the source reaches the DOM. Links and images resolve to the document's repository and revision. -->
<script lang="ts">
	import {
		parseDocument,
		resolveImage,
		resolveLink,
		type DocBlock,
		type DocLocation,
		type Inline
	} from './markdown.ts';

	let { source, repo, rev, path }: { source: string } & DocLocation = $props();

	const blocks = $derived(parseDocument(source));
	const loc = $derived<DocLocation>({ repo, rev, path });

	let root = $state<HTMLElement | null>(null);

	// Scroll to the heading named in the URL hash once the document is rendered.
	$effect(() => {
		void blocks;
		if (!root) return;
		const id = decodeURIComponent(location.hash.slice(1));
		if (!id) return;
		const el = root.ownerDocument.getElementById(id);
		if (el && root.contains(el)) el.scrollIntoView();
	});

	function external(href: string): boolean {
		return /^(?:https?:)?\/\//i.test(href);
	}
</script>

{#snippet inline(nodes: Inline[])}
	{#each nodes as n, i (i)}
		{#if n.kind === 'text'}{n.text}{:else if n.kind === 'code'}<code>{n.text}</code
			>{:else if n.kind === 'strong'}<strong>{@render inline(n.children)}</strong
			>{:else if n.kind === 'em'}<em>{@render inline(n.children)}</em
			>{:else if n.kind === 'del'}<del>{@render inline(n.children)}</del
			>{:else if n.kind === 'link'}{@const href = resolveLink(n.href, loc)}{#if href}<a
					{href}
					rel={external(href) ? 'noopener noreferrer' : undefined}>{@render inline(n.children)}</a
				>{:else}{@render inline(n.children)}{/if}{:else if n.kind === 'image'}{@const img = resolveImage(n.src, loc)}{#if img.kind === 'image'}<img
					src={img.src}
					alt={n.alt}
					loading="lazy"
				/>{:else if img.kind === 'link'}<a
					href={img.href}
					title="Image: {img.href}"
					rel={external(img.href) ? 'noopener noreferrer' : undefined}>{n.alt || 'image'}</a
				>{:else}{n.alt}{/if}{/if}
	{/each}
{/snippet}

{#snippet list(items: DocBlock[], tight: boolean)}
	{#each items as b, i (i)}
		{#if b.kind === 'heading'}
			<svelte:element this={`h${b.level}`} id={b.id}>{@render inline(b.children)}</svelte:element>
		{:else if b.kind === 'para'}
			{#if tight}<span class="tight">{@render inline(b.children)}</span>{:else}<p>{@render inline(b.children)}</p>{/if}
		{:else if b.kind === 'code'}
			<pre data-lang={b.lang || undefined}><code>{b.text}</code></pre>
		{:else if b.kind === 'rule'}
			<hr />
		{:else if b.kind === 'quote'}
			<blockquote>{@render list(b.children, false)}</blockquote>
		{:else if b.kind === 'table'}
			<div class="table-wrap">
				<table>
					<thead>
						<tr>
							{#each b.head as cell, c (c)}<th style:text-align={b.align[c]}>{@render inline(cell)}</th>{/each}
						</tr>
					</thead>
					<tbody>
						{#each b.rows as row, r (r)}
							<tr>
								{#each row as cell, c (c)}<td style:text-align={b.align[c]}>{@render inline(cell)}</td>{/each}
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else if b.ordered}
			<ol start={b.start === 1 ? undefined : b.start}>
				{#each b.items as item, j (j)}<li class:task={item.task !== null}
						>{#if item.task !== null}<input type="checkbox" checked={item.task} disabled />
						{/if}{@render list(item.children, b.tight)}</li
					>{/each}
			</ol>
		{:else}
			<ul>
				{#each b.items as item, j (j)}<li class:task={item.task !== null}
						>{#if item.task !== null}<input type="checkbox" checked={item.task} disabled />
						{/if}{@render list(item.children, b.tight)}</li
					>{/each}
			</ul>
		{/if}
	{/each}
{/snippet}

<div class="markdown-doc" bind:this={root}>
	{@render list(blocks, false)}
</div>

<style>
	.markdown-doc {
		overflow-wrap: break-word;
		line-height: 1.6;
		max-width: 60rem;
	}
	.markdown-doc > :global(:first-child) {
		margin-top: 0;
	}
	.markdown-doc > :global(:last-child) {
		margin-bottom: 0;
	}
	p,
	blockquote,
	pre,
	ul,
	ol,
	.table-wrap {
		margin: 0 0 var(--space-3);
	}
	p,
	.tight {
		white-space: pre-line;
	}
	h1,
	h2,
	h3,
	h4,
	h5,
	h6 {
		margin: var(--space-6) 0 var(--space-3);
		font-weight: 600;
		line-height: 1.25;
	}
	h1,
	h2 {
		padding-bottom: var(--space-2);
		border-bottom: 1px solid var(--color-border);
	}
	h1 {
		font-size: 1.75em;
	}
	h2 {
		font-size: 1.4em;
	}
	h3 {
		font-size: 1.2em;
	}
	h4 {
		font-size: 1em;
	}
	h5,
	h6 {
		font-size: 0.9em;
	}
	h6 {
		color: var(--color-text-muted);
	}
	code {
		font-family: var(--font-mono);
		font-size: 0.9em;
		padding: 0.1em 0.3em;
		background: var(--color-bg-subtle);
		border-radius: var(--radius);
	}
	pre {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-3);
		overflow-x: auto;
		line-height: 1.45;
	}
	pre code {
		padding: 0;
		background: none;
		font-size: var(--text-sm);
	}
	blockquote {
		padding-left: var(--space-3);
		border-left: 3px solid var(--color-border-strong);
		color: var(--color-text-muted);
	}
	ul,
	ol {
		padding-left: var(--space-6);
	}
	li > :global(ul),
	li > :global(ol) {
		margin-bottom: 0;
	}
	li.task {
		list-style: none;
	}
	li.task input {
		margin: 0 var(--space-1) 0 calc(-1 * var(--space-4));
		vertical-align: middle;
	}
	hr {
		border: 0;
		border-top: 1px solid var(--color-border);
		margin: var(--space-4) 0;
	}
	.table-wrap {
		overflow-x: auto;
	}
	table {
		border-collapse: collapse;
	}
	th,
	td {
		border: 1px solid var(--color-border);
		padding: var(--space-1) var(--space-3);
	}
	th {
		font-weight: 600;
		background: var(--color-bg-subtle);
	}
	img {
		max-width: 100%;
		vertical-align: middle;
	}
</style>
