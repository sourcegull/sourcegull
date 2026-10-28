<script lang="ts">
	// Compute: runs a compute query and streams its results.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onDestroy } from 'svelte';
	import ReplaceDiff from '#lib/search/ReplaceDiff.svelte';
	import {
		combinedOutput,
		exportText,
		isMatchContext,
		resultFileURL,
		streamCompute,
		type ComputeResult
	} from '#lib/search/compute.ts';
	import type { Alert, Progress } from '#lib/search/stream.ts';

	const EXAMPLES = [
		'repo:myrepo content:output((\\w+)Error -> $1) type:file',
		'content:output.extra(TODO\\((\\w+)\\) -> $1) separator:,',
		'lang:go content:replace(fmt.Println -> log.Println)',
		'type:commit content:output(.* -> $author) count:all'
	];

	let text = $state(page.url.searchParams.get('q') ?? '');
	let results = $state.raw<ComputeResult[]>([]);
	let alerts = $state.raw<Alert[]>([]);
	let progress = $state<Progress | null>(null);
	let error = $state<string | null>(null);
	let running = $state(false);
	let controller: AbortController | null = null;

	/** Output values joined across results, for the notebook-style view. */
	const combined = $derived(combinedOutput(results));
	/** What Copy and Download give. */
	const exported = $derived(exportText(results));
	let copied = $state(false);
	let copyTimer: ReturnType<typeof setTimeout> | undefined;

	async function copy(): Promise<void> {
		try {
			await navigator.clipboard.writeText(exported);
			copied = true;
			clearTimeout(copyTimer);
			copyTimer = setTimeout(() => (copied = false), 2000);
		} catch {
			copied = false;
		}
	}

	function download(): void {
		const url = URL.createObjectURL(new Blob([exported], { type: 'text/plain' }));
		const a = document.createElement('a');
		a.href = url;
		a.download = 'compute.txt';
		a.click();
		setTimeout(() => URL.revokeObjectURL(url), 1000);
	}

	function run(query: string): void {
		controller?.abort();
		const c = new AbortController();
		controller = c;
		results = [];
		alerts = [];
		progress = null;
		error = null;
		if (!query.trim()) return;
		running = true;
		void streamCompute(
			query,
			{
				onResults: (r) => (results = [...results, ...r]),
				onProgress: (p) => (progress = p),
				onAlert: (a) => (alerts = [...alerts, a]),
				onError: (e) => (error = e),
				onDone: () => (running = false)
			},
			c.signal
		).finally(() => {
			if (controller === c) running = false;
		});
	}

	$effect(() => {
		run(page.url.searchParams.get('q') ?? '');
	});

	onDestroy(() => {
		controller?.abort();
		clearTimeout(copyTimer);
	});

	function submit(e: SubmitEvent): void {
		e.preventDefault();
		const q = new URLSearchParams({ q: text });
		void goto(`/compute?${q.toString()}`);
	}
</script>

<svelte:head><title>Compute</title></svelte:head>

<div class="compute-page">
	<div class="page-header">
		<h1>Compute</h1>
	</div>
	<form class="toolbar" onsubmit={submit}>
		<label class="visually-hidden" for="compute-q">Compute query</label>
		<input id="compute-q" class="input query" type="search" placeholder="content:output(pattern -> template)" bind:value={text} />
		<button type="submit" class="btn btn-primary">Run</button>
		{#if running}<button type="button" class="btn" onclick={() => controller?.abort()}>Stop</button>{/if}
	</form>
	<p class="muted hint">
		A search query whose pattern is a regexp, or <code>content:output(pattern -> template)</code>,
		<code>content:output.extra(…)</code>, <code>content:replace(pattern -> template)</code> (add
		<code>.structural</code> for structural patterns). Templates use <code>$1</code>, <code>$name</code>,
		<code>:[hole]</code>, <code>$repo</code>, <code>$path</code>, <code>$lang</code>, <code>$author</code>,
		<code>$date</code>, <code>$content</code>.
	</p>
	{#if !page.url.searchParams.get('q')}
		<ul class="examples">
			{#each EXAMPLES as ex (ex)}
				<li><a href={`/compute?${new URLSearchParams({ q: ex }).toString()}`}><code>{ex}</code></a></li>
			{/each}
		</ul>
	{/if}

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#each alerts as a (a.title + a.description)}
		<div class="alert alert-warning" role="status"><strong>{a.title}</strong> {a.description}</div>
	{/each}
	{#if progress || running}
		<p class="muted status" aria-live="polite">
			{running ? 'Computing…' : 'Done.'}
			{#if progress}{progress.matchCount} search results{#if progress.repositoriesCount != null} in {progress.repositoriesCount} repositories{/if}, {results.length} computed results, {progress.durationMs} ms{/if}
		</p>
	{/if}

	{#if exported}
		<div class="output-header">
			<h2>{combined ? 'Output' : 'Matches'}</h2>
			<span class="spacer"></span>
			<button type="button" class="btn btn-sm btn-ghost" onclick={copy}>
				{copied ? 'Copied' : 'Copy'}
			</button>
			<button type="button" class="btn btn-sm btn-ghost" onclick={download}>
				Download
			</button>
		</div>
		{#if combined}<pre class="card output">{combined}</pre>{/if}
	{/if}

	<ul class="results">
		{#each results as r, i (i)}
			{#if isMatchContext(r)}
				<li class="card">
					<div class="where"><a href={resultFileURL(r)}><code>{r.repository}</code> › <code>{r.path}</code></a></div>
					<table>
						<tbody>
							{#each r.matches as m, j (j)}
								<tr>
									<td class="muted"><a href={`${resultFileURL(r)}#L${m.range.start.line + 1}`}>{m.range.start.line + 1}</a></td>
									<td><code>{m.value}</code></td>
									<td>
										{#each Object.entries(m.environment) as [k, d] (k)}
											<span class="badge">{k}: {d.value}</span>
										{/each}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</li>
			{:else if r.kind === 'replace-in-place'}
				<li class="replace">
					<ReplaceDiff result={r} />
				</li>
			{:else if r.kind !== 'output'}
				<li class="card">
					<div class="where muted">{r.kind}{#if r.repository} · <code>{r.repository}</code>{/if}</div>
					<pre>{r.value}</pre>
				</li>
			{/if}
		{/each}
	</ul>
</div>

<style>
	.compute-page {
		max-width: 70rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
	}
	.query {
		flex: 1;
		font-family: var(--font-mono, monospace);
	}
	.hint,
	.status {
		font-size: 0.85rem;
	}
	.output-header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.spacer {
		flex: 1;
	}
	.examples {
		padding-left: var(--space-4);
	}
	.results {
		list-style: none;
		padding: 0;
		display: grid;
		gap: var(--space-3);
	}
	.results li.card,
	.output {
		padding: var(--space-3);
		overflow-x: auto;
	}
	.where {
		margin-bottom: var(--space-2);
		overflow-wrap: anywhere;
	}
	pre {
		margin: 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	td {
		padding: 0 var(--space-2);
		vertical-align: top;
	}
</style>
