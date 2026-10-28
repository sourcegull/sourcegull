<!--
	A symbol block's view: the symbol is looked up at the block's revision (the default branch
	when none) with a symbol search, in the recorded file and then anywhere in the repository,
	and its definition is shown with `lineContext` lines around it. When it is gone, a message
	says so and links to the file.
-->
<script lang="ts">
	import { repoPageURL } from '#lib/repo/url.ts';
	import { streamSearch, type SymbolMatch } from '#lib/search/stream.ts';
	import CodeSnippet from './CodeSnippet.svelte';
	import type { ResolvedSymbol, SymbolBlockInput } from './serialize.ts';
	import { pickSymbol, symbolSearchQuery, type FoundSymbol } from './symbols.ts';

	interface Props {
		input: SymbolBlockInput;
		/** Called with where the symbol was found (null when it was not). */
		onresolved?: (r: ResolvedSymbol | null) => void;
	}

	let { input, onresolved }: Props = $props();

	let found = $state.raw<FoundSymbol | null>(null);
	let status = $state<'loading' | 'found' | 'moved' | 'gone' | 'error'>('loading');
	let error = $state<string | null>(null);

	async function search(query: string, signal: AbortSignal): Promise<SymbolMatch[]> {
		const out: SymbolMatch[] = [];
		const failures: string[] = [];
		await streamSearch(
			{ query, patternType: 'regexp', display: 200 },
			{
				onMatches: (ms) => {
					for (const m of ms) if (m.type === 'symbol') out.push(m);
				},
				onError: (m) => {
					failures.push(m);
				}
			},
			signal
		);
		if (failures.length > 0) throw new Error(failures.join('; '));
		return out;
	}

	$effect(() => {
		const s = { ...input };
		const controller = new AbortController();
		status = 'loading';
		found = null;
		error = null;
		void (async () => {
			try {
				let hit = pickSymbol(s, await search(symbolSearchQuery(s, true), controller.signal));
				if (!hit && !controller.signal.aborted) {
					hit = pickSymbol(s, await search(symbolSearchQuery(s, false), controller.signal));
				}
				if (controller.signal.aborted) return;
				found = hit;
				status = !hit ? 'gone' : hit.path === s.filePath ? 'found' : 'moved';
				onresolved?.(
					hit
						? {
								revision: s.revision,
								filePath: hit.path,
								range: { startLine: hit.line, startCol: hit.startCol, endLine: hit.line, endCol: hit.endCol }
							}
						: null
				);
			} catch (err) {
				if (controller.signal.aborted) return;
				status = 'error';
				error = err instanceof Error ? err.message : String(err);
				onresolved?.(null);
			}
		})();
		return () => controller.abort();
	});

	const lines = $derived(
		found
			? {
					startLine: Math.max(0, found.line - 1 - input.lineContext),
					endLine: found.line + input.lineContext
				}
			: null
	);
	const fileURL = $derived(repoPageURL(input.repositoryName, input.revision || null, 'blob', input.filePath));
</script>

<div class="symbol-block">
	<div class="symbol-header">
		<span class="badge" title={input.symbolKind}>{input.symbolKind.toLowerCase()}</span>
		<span class="mono name">{input.symbolName}</span>
		{#if input.symbolContainerName}<span class="muted mono">in {input.symbolContainerName}</span>{/if}
	</div>
	{#if status === 'loading'}
		<p class="muted">Looking up the symbol…</p>
	{:else if status === 'error'}
		<div class="alert alert-danger" role="alert">Looking up the symbol failed: {error}</div>
	{:else if status === 'gone' || !found || !lines}
		<div class="alert alert-warning" role="status">
			The symbol <code>{input.symbolName}</code> was not found in {input.repositoryName}{input.revision
				? `@${input.revision}`
				: ''}: it may have been renamed or removed. <a href={fileURL}>Open {input.filePath}</a>.
		</div>
	{:else}
		{#if status === 'moved'}
			<p class="muted moved">
				Not in {input.filePath} any more; showing its definition in <span class="mono">{found.path}</span>.
			</p>
		{/if}
		<CodeSnippet
			repo={input.repositoryName}
			rev={found.commit}
			path={found.path}
			{lines}
			selection={{ start: found.line, end: found.line }}
		/>
	{/if}
</div>

<style>
	.symbol-block {
		display: grid;
		gap: var(--space-2);
	}
	.symbol-header {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.name {
		font-weight: 600;
	}
	.moved {
		margin: 0;
		font-size: 0.85rem;
	}
	.alert {
		margin: 0;
	}
</style>
