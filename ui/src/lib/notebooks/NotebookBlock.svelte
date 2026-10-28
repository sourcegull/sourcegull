<!--
	One notebook block: its view (rendered Markdown, live query results, a highlighted file
	range, a resolved symbol) and, for users who can manage the notebook, its editor and the
	block actions (move, duplicate, delete).
-->
<script lang="ts">
	import Markdown from '#lib/batches/Markdown.svelte';
	import { EVENTS, logEvent } from '#lib/events.ts';
	import QueryInput from '#lib/search/QueryInput.svelte';
	import { parsePatternType } from '#lib/search/query.ts';
	import { streamSearch, type SymbolMatch } from '#lib/search/stream.ts';
	import CodeSnippet from './CodeSnippet.svelte';
	import QueryResults from './QueryResults.svelte';
	import { SYMBOL_KINDS } from './queries.ts';
	import {
		DEFAULT_LINE_CONTEXT,
		blockFromURL,
		parseBlockLineRange,
		serializeLineRange,
		type Block,
		type ResolvedSymbol
	} from './serialize.ts';
	import SymbolSnippet from './SymbolSnippet.svelte';

	interface Props {
		block: Block;
		index: number;
		count: number;
		canManage: boolean;
		patternType: string;
		editing: boolean;
		onedit: (editing: boolean) => void;
		onchange: (block: Block) => void;
		onmove: (delta: number) => void;
		onduplicate: () => void;
		ondelete: () => void;
		onresolved?: (r: ResolvedSymbol | null) => void;
	}

	let {
		block,
		index,
		count,
		canManage,
		patternType,
		editing,
		onedit,
		onchange,
		onmove,
		onduplicate,
		ondelete,
		onresolved
	}: Props = $props();

	const LABELS: Record<Block['type'], string> = { md: 'Markdown', query: 'Query', file: 'File', symbol: 'Symbol' };

	// The editor works on a draft copied from the block when editing starts.
	let draft = $state<Block | null>(null);
	let lineRangeText = $state('');
	let urlText = $state('');
	let formError = $state<string | null>(null);
	let runKey = $state(0);

	$effect(() => {
		if (editing && !draft) {
			draft = structuredClone($state.snapshot(block)) as Block;
			lineRangeText = block.type === 'file' ? serializeLineRange(block.input.lineRange) : '';
			urlText = '';
			formError = null;
		} else if (!editing && draft) {
			draft = null;
		}
	});

	function save(): void {
		if (!draft) return;
		formError = null;
		const b = $state.snapshot(draft) as Block;
		if (b.type === 'file') {
			const t = lineRangeText.trim();
			const r = t ? parseBlockLineRange(t) : null;
			if (t && !r) {
				formError = 'Lines must be a line number or a range like 10-20.';
				return;
			}
			b.input.lineRange = r;
			if (!b.input.repositoryName.trim() || !b.input.filePath.trim()) {
				formError = 'A repository and a file path are required.';
				return;
			}
		}
		if (b.type === 'symbol' && (!b.input.repositoryName.trim() || !b.input.filePath.trim() || !b.input.symbolName.trim())) {
			formError = 'Pick a symbol, or fill in its repository, file and name.';
			return;
		}
		onchange(b);
		onedit(false);
	}

	function fromURL(): void {
		if (!draft) return;
		let text = urlText.trim();
		if (text.startsWith('/')) text = location.origin + text;
		const parsed = blockFromURL(text);
		if (!parsed || (parsed.type !== 'file' && parsed.type !== 'symbol')) {
			formError = 'Not a link to a file.';
			return;
		}
		formError = null;
		if (draft.type === 'file') {
			const input = parsed.type === 'file' ? parsed.input : { ...parsed.input, lineRange: null };
			draft = { id: draft.id, type: 'file', input: { repositoryName: input.repositoryName, revision: input.revision, filePath: input.filePath, lineRange: input.lineRange } };
			lineRangeText = serializeLineRange(input.lineRange);
		} else if (draft.type === 'symbol' && parsed.type === 'symbol') {
			draft = { id: draft.id, type: 'symbol', input: parsed.input };
		} else if (draft.type === 'symbol') {
			draft = { ...draft, input: { ...draft.input, repositoryName: parsed.input.repositoryName, revision: parsed.input.revision, filePath: parsed.input.filePath } };
		}
	}

	// Symbol picker: a symbol search whose results fill the symbol block.
	let symbolQuery = $state('');
	let symbolResults = $state.raw<{ match: SymbolMatch; index: number }[]>([]);
	let symbolSearching = $state(false);
	let symbolController: AbortController | null = null;

	function findSymbols(): void {
		symbolController?.abort();
		const q = symbolQuery.trim();
		symbolResults = [];
		if (!q) return;
		const controller = new AbortController();
		symbolController = controller;
		symbolSearching = true;
		const found: { match: SymbolMatch; index: number }[] = [];
		void streamSearch(
			{ query: /\btype:symbol\b/.test(q) ? q : `type:symbol ${q}`, patternType: parsePatternType(patternType), display: 30 },
			{
				onMatches: (ms) => {
					for (const m of ms) if (m.type === 'symbol') m.symbols.forEach((_, i) => found.push({ match: m, index: i }));
					symbolResults = found.slice(0, 30);
				}
			},
			controller.signal
		).finally(() => {
			if (symbolController === controller) symbolSearching = false;
		});
	}

	function pick(m: SymbolMatch, i: number): void {
		if (!draft || draft.type !== 'symbol') return;
		const s = m.symbols[i];
		if (!s) return;
		const branch = m.branches?.[0];
		draft = {
			id: draft.id,
			type: 'symbol',
			input: {
				repositoryName: m.repository,
				revision: branch && branch !== 'HEAD' ? branch : '',
				filePath: m.path,
				symbolName: s.name,
				symbolContainerName: s.containerName,
				symbolKind: s.kind || 'UNKNOWN',
				lineContext: draft.input.lineContext ?? DEFAULT_LINE_CONTEXT
			}
		};
		symbolResults = [];
	}

	function run(): void {
		runKey++;
		logEvent(EVENTS.notebookBlockRun, { type: block.type });
	}
</script>

<section class="block" class:editing aria-label="{LABELS[block.type]} block {index + 1}">
	{#if canManage}
		<div class="actions">
			<span class="kind muted">{LABELS[block.type]}</span>
			<span class="spacer"></span>
			{#if block.type === 'query' && !editing}
				<button type="button" class="btn btn-sm" onclick={run}>Run</button>
			{/if}
			{#if !editing}
				<button type="button" class="btn btn-sm" onclick={() => onedit(true)}>Edit</button>
			{/if}
			<button type="button" class="btn btn-sm" title="Move up" aria-label="Move block up" disabled={index === 0} onclick={() => onmove(-1)}>↑</button>
			<button type="button" class="btn btn-sm" title="Move down" aria-label="Move block down" disabled={index === count - 1} onclick={() => onmove(1)}>↓</button>
			<button type="button" class="btn btn-sm" onclick={onduplicate}>Duplicate</button>
			<button type="button" class="btn btn-sm btn-danger" onclick={ondelete}>Delete</button>
		</div>
	{/if}

	{#if editing && draft}
		<form
			class="editor"
			onsubmit={(e) => {
				e.preventDefault();
				save();
			}}
		>
			{#if draft.type === 'md'}
				<label class="visually-hidden" for="md-{block.id}">Markdown</label>
				<textarea id="md-{block.id}" class="input mono" rows="8" bind:value={draft.input.text} placeholder="Markdown text…"></textarea>
			{:else if draft.type === 'query'}
				<QueryInput
					bind:value={draft.input.query}
					patternType={parsePatternType(patternType)}
					label="Query"
					placeholder="Search query…"
					onsubmit={save}
				/>
			{:else}
				<div class="row">
					<label class="visually-hidden" for="url-{block.id}">File link</label>
					<input id="url-{block.id}" class="input" placeholder="Paste a link to a file or line range…" bind:value={urlText} />
					<button type="button" class="btn" onclick={fromURL}>Use link</button>
				</div>
				{#if draft.type === 'symbol'}
					<div class="row">
						<label class="visually-hidden" for="sym-{block.id}">Find a symbol</label>
						<input
							id="sym-{block.id}"
							class="input"
							placeholder="Find a symbol (e.g. repo:acme/web Handler)…"
							bind:value={symbolQuery}
							onkeydown={(e) => {
								if (e.key === 'Enter') {
									e.preventDefault();
									findSymbols();
								}
							}}
						/>
						<button type="button" class="btn" onclick={findSymbols} disabled={symbolSearching}>Find</button>
					</div>
					{#if symbolResults.length > 0}
						<ul class="picker">
							{#each symbolResults as r (`${r.match.repository}:${r.match.path}:${r.index}`)}
								{@const s = r.match.symbols[r.index]}
								{#if s}
									<li>
										<button type="button" class="link" onclick={() => pick(r.match, r.index)}>
											<span class="badge">{s.kind.toLowerCase()}</span>
											<span class="mono">{s.name}</span>
											<span class="muted">{r.match.repository} › {r.match.path}:{s.line}</span>
										</button>
									</li>
								{/if}
							{/each}
						</ul>
					{/if}
				{/if}
				<div class="grid">
					<label>Repository <input class="input mono" required bind:value={draft.input.repositoryName} /></label>
					<label>File path <input class="input mono" required bind:value={draft.input.filePath} /></label>
					<label>Revision <input class="input mono" placeholder="default branch" bind:value={draft.input.revision} /></label>
					{#if draft.type === 'file'}
						<label>Lines <input class="input mono" placeholder="all, or 10-20" bind:value={lineRangeText} /></label>
					{:else}
						<label>Symbol name <input class="input mono" required bind:value={draft.input.symbolName} /></label>
						<label>Container <input class="input mono" bind:value={draft.input.symbolContainerName} /></label>
						<label
							>Kind
							<select class="input" bind:value={draft.input.symbolKind}>
								{#each SYMBOL_KINDS as k (k)}<option value={k}>{k.toLowerCase()}</option>{/each}
							</select>
						</label>
						<label>Lines of context <input class="input" type="number" min="0" max="1000" bind:value={draft.input.lineContext} /></label>
					{/if}
				</div>
			{/if}
			{#if formError}<div class="alert alert-danger" role="alert">{formError}</div>{/if}
			<div class="row">
				<button type="submit" class="btn btn-primary btn-sm">Done</button>
				<button type="button" class="btn btn-sm" onclick={() => onedit(false)}>Cancel</button>
			</div>
		</form>
	{/if}

	<div class="view">
		{#if block.type === 'md'}
			{#if block.input.text.trim()}
				<Markdown source={block.input.text} />
			{:else if canManage && !editing}
				<p class="muted">Empty Markdown block. <button type="button" class="link" onclick={() => onedit(true)}>Edit</button></p>
			{/if}
		{:else if block.type === 'query'}
			{#if !editing}
				<pre class="query mono">{block.input.query}</pre>
				<QueryResults query={block.input.query} {patternType} {runKey} />
			{/if}
		{:else if block.type === 'file'}
			{#if block.input.repositoryName && block.input.filePath}
				<CodeSnippet
					repo={block.input.repositoryName}
					rev={block.input.revision}
					path={block.input.filePath}
					lines={block.input.lineRange}
				/>
			{:else if !editing}
				<p class="muted">No file chosen yet.</p>
			{/if}
		{:else if block.type === 'symbol'}
			{#if block.input.repositoryName && block.input.symbolName}
				<SymbolSnippet input={block.input} {onresolved} />
			{:else if !editing}
				<p class="muted">No symbol chosen yet.</p>
			{/if}
		{/if}
	</div>
</section>

<style>
	.block {
		position: relative;
		padding: var(--space-3);
		border: 1px solid transparent;
		border-radius: var(--radius, 4px);
		display: grid;
		gap: var(--space-2);
	}
	.block:hover,
	.block:focus-within,
	.block.editing {
		border-color: var(--color-border);
	}
	.actions {
		display: flex;
		gap: var(--space-1);
		align-items: center;
		font-size: 0.8rem;
		opacity: 0.4;
	}
	.block:hover .actions,
	.block:focus-within .actions,
	.block.editing .actions {
		opacity: 1;
	}
	.kind {
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
	.spacer {
		flex: 1;
	}
	.editor {
		display: grid;
		gap: var(--space-2);
	}
	.row {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.row .input {
		flex: 1;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
		gap: var(--space-2);
	}
	.grid label {
		display: grid;
		gap: var(--space-1);
		font-size: 0.85rem;
	}
	textarea {
		width: 100%;
		resize: vertical;
	}
	.picker {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 14rem;
		overflow: auto;
		border: 1px solid var(--color-border);
	}
	.picker li button {
		display: flex;
		gap: var(--space-2);
		width: 100%;
		text-align: left;
		padding: var(--space-1) var(--space-2);
	}
	.query {
		margin: 0;
		padding: var(--space-2);
		background: var(--color-bg-subtle);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.link {
		background: none;
		border: none;
		padding: 0;
		color: var(--color-link, inherit);
		cursor: pointer;
	}
	.alert {
		margin: 0;
	}
</style>
