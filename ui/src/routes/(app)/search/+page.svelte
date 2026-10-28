<script lang="ts">
	import type { CreateSavedSearchMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount, tick } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { createInsightFromSearchURL } from '#lib/insights/initial.ts';
	import Aggregations from '#lib/search/Aggregations.svelte';
	import ContextPicker from '#lib/search/ContextPicker.svelte';
	import FilterSidebar from '#lib/search/FilterSidebar.svelte';
	import QueryInput, { type ValueSuggester } from '#lib/search/QueryInput.svelte';
	import SearchResult from '#lib/search/SearchResult.svelte';
	import QueryText from '#lib/search/QueryText.svelte';
	import Icon from '#lib/components/Icon.svelte';
	import {
		recentQuery,
		recentSearches,
		relativeTime,
		resultCountLabel,
		type RecentSearch
	} from '#lib/search/recent-searches.ts';
	import {
		CREATE_SAVED_SEARCH,
		DEFAULT_SEARCH_CONTEXT,
		REPOSITORY_NAMES,
		SEARCH_CONTEXTS,
		type CreateSavedSearchVariables,
		type DefaultSearchContextResult,
		type RepositoryNamesResult,
		type SearchContextsResult,
		type SearchContextsVariables
	} from '#lib/search/queries.ts';
	import {
		PATTERN_TYPES,
		appendFilter,
		exactRepo,
		parsePatternType,
		removeFilter,
		searchQueryURL,
		searchURL,
		splitContext,
		withContext,
		withPatternType,
		type PatternType,
		type QueryDiagnostic
	} from '#lib/search/query.ts';
	import { computePageURL } from '#lib/search/compute.ts';
	import { CREATE_SEARCH_JOB, VALIDATE_SEARCH_JOB } from '#lib/search/search-jobs.ts';
	import {
		matchCount,
		matchKey,
		streamSearch,
		type Alert,
		type Filter,
		type Progress,
		type SearchMatch
	} from '#lib/search/stream.ts';

	/** The most results the stream sends. */
	const DISPLAY_LIMIT = 1500;
	/** Results rendered at first and per scroll step. */
	const RENDER_STEP = 50;

	const user = $derived(page.data.auth?.user ?? null);

	// Recent searches: on the server for signed-in users, in local storage otherwise.
	const recent = $derived(recentSearches(user?.id ?? null));
	$effect(() => {
		void recent.load();
	});

	// The query in the URL, split into the context picker and the query box.
	const urlQuery = $derived(page.url.searchParams.get('q'));
	const urlPatternType = $derived(parsePatternType(page.url.searchParams.get('patternType')));

	let text = $state('');
	let context = $state<string | null>(null);
	let patternType = $state<PatternType>('keyword');
	let defaultContext = $state<string | null>(null);
	let diagnostics = $state<QueryDiagnostic[]>([]);

	let results = $state.raw<SearchMatch[]>([]);
	let filters = $state.raw<Filter[]>([]);
	let alerts = $state.raw<Alert[]>([]);
	let progress = $state.raw<Progress | null>(null);
	let streamError = $state<string | null>(null);
	let running = $state(false);
	let rendered = $state(RENDER_STEP);
	let showAggregations = $state(false);
	let submitted = $state<{ query: string; patternType: PatternType } | null>(null);

	let input = $state<QueryInput>();
	let sentinel = $state<HTMLDivElement>();
	let resultsList = $state<HTMLOListElement>();
	let controller: AbortController | undefined;

	// Saved search form.
	let saving = $state(false);
	let saveDescription = $state('');
	let saveError = $state<string | null>(null);
	let savedURL = $state<string | null>(null);

	// "Create search job": the query is validated when the form opens.
	let jobForm = $state(false);
	let jobQuery = $state('');
	let jobInvalid = $state<string | null>(null);
	let jobError = $state<string | null>(null);
	let jobBusy = $state(false);

	async function openJob(): Promise<void> {
		if (!submitted) return;
		jobForm = true;
		jobError = null;
		jobInvalid = null;
		jobQuery = withPatternType(submitted.query, submitted.patternType);
		jobBusy = true;
		try {
			await graphql(VALIDATE_SEARCH_JOB, { query: jobQuery });
		} catch (err) {
			jobInvalid = errorMessage(err);
		} finally {
			jobBusy = false;
		}
	}

	async function createJob(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		jobBusy = true;
		jobError = null;
		try {
			await graphql(CREATE_SEARCH_JOB, { query: jobQuery });
			await goto('/search-jobs');
		} catch (err) {
			jobError = errorMessage(err);
		} finally {
			jobBusy = false;
		}
	}

	onMount(() => {
		if (!user) return;
		graphql<DefaultSearchContextResult>(DEFAULT_SEARCH_CONTEXT)
			.then((r) => {
				defaultContext = r.defaultSearchContext?.spec ?? null;
				if (urlQuery === null && context === null) context = defaultContext;
			})
			.catch(() => {});
		return () => controller?.abort();
	});

	// Navigation (submit, back/forward, filter clicks) drives the search.
	$effect(() => {
		const q = urlQuery;
		const pt = urlPatternType;
		patternType = pt;
		if (q === null) {
			controller?.abort();
			submitted = null;
			text = '';
			context = defaultContext;
			return;
		}
		const split = splitContext(q);
		text = split.rest;
		context = split.context;
		run(q, pt);
	});

	function run(query: string, pt: PatternType): void {
		controller?.abort();
		const c = new AbortController();
		controller = c;
		submitted = { query, patternType: pt };
		results = [];
		filters = [];
		alerts = [];
		progress = null;
		streamError = null;
		rendered = RENDER_STEP;
		savedURL = null;
		saving = false;
		jobForm = false;
		running = true;
		void streamSearch(
			{ query, patternType: pt, display: DISPLAY_LIMIT },
			{
				onMatches: (m) => {
					if (!c.signal.aborted) results = results.concat(m);
				},
				onProgress: (p) => {
					if (!c.signal.aborted) progress = p;
				},
				onFilters: (f) => {
					if (!c.signal.aborted) filters = f;
				},
				onAlert: (a) => {
					if (!c.signal.aborted) alerts = [...alerts, a];
				},
				onError: (e) => {
					if (!c.signal.aborted) streamError = e;
				},
				onDone: () => {}
			},
			c.signal
		).finally(() => {
			if (controller === c) running = false;
			if (c.signal.aborted) return;
			void recent.add({
				query: recentQuery(query, pt),
				resultCount: results.reduce((n, m) => n + matchCount(m), 0),
				limitHit: (progress?.skipped ?? []).some((s) => s.reason === 'display'),
				timestamp: new Date().toISOString()
			});
		});
	}

	function submit(): void {
		const q = withContext(text, context);
		if (!q.trim()) return;
		const url = searchURL(q, patternType);
		if (url === page.url.pathname + page.url.search) run(q, patternType);
		else void goto(url);
	}

	function changeFilters(remove: string[], add: string[]): void {
		let q = text;
		for (const r of remove) q = removeFilter(q, r);
		for (const a of add) q = appendFilter(q, a);
		text = q;
		submit();
	}

	function selectContext(spec: string): void {
		context = spec;
		if (submitted) submit();
		else input?.focus();
	}

	function selectRecent(entry: RecentSearch): void {
		const url = searchQueryURL(entry.query);
		if (url === page.url.pathname + page.url.search) {
			const split = splitContext(entry.query);
			text = split.rest;
			context = split.context;
			run(withContext(split.rest, split.context), patternType);
		} else void goto(url);
	}

	function runQuery(q: string): void {
		void goto(searchURL(q, patternType));
	}

	const suggest: ValueSuggester = async (field, prefix) => {
		if (field === 'context') {
			const r = await graphql<SearchContextsResult, SearchContextsVariables>(SEARCH_CONTEXTS, {
				first: 10,
				query: prefix || null
			});
			return r.searchContexts.nodes.map((c) => c.spec);
		}
		if (field === 'repo') {
			const r = await graphql<RepositoryNamesResult, { query: string | null; first: number }>(
				REPOSITORY_NAMES,
				{ query: prefix.replace(/^\^|\$$/g, '').replace(/\\/g, '') || null, first: 10 }
			);
			return r.repositories.nodes.map((n) => exactRepo(n.name));
		}
		return [];
	};

	// Render more results as the end of the list scrolls into view.
	$effect(() => {
		const el = sentinel;
		if (!el) return;
		const io = new IntersectionObserver((entries) => {
			if (entries.some((e) => e.isIntersecting)) rendered += RENDER_STEP;
		}, { rootMargin: '600px' });
		io.observe(el);
		return () => io.disconnect();
	});

	const resultCount = $derived(results.reduce((n, m) => n + matchCount(m), 0));
	const skipped = $derived(progress?.skipped ?? []);

	function seconds(ms: number): string {
		return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(2)} s`;
	}

	/** Below 52rem the filter sidebar is a panel toggled from the toolbar. */
	let filtersOpen = $state(false);
	const limitHit = $derived(skipped.some((s) => s.reason === 'display'));
	const incomplete = $derived(skipped.some((s) => s.severity === 'warn'));

	const EXAMPLES: { query: string; description: string }[] = [
		{ query: 'lang:rust fn main', description: 'Rust entry points' },
		{ query: 'type:symbol Parser', description: 'Symbols named Parser' },
		{ query: 'type:commit message:fix after:"1 month ago"', description: 'Recent fix commits' },
		{ query: 'type:diff select:commit.diff.added TODO', description: 'TODOs added in diffs' },
		{ query: 'repo:has.file(path:go.mod) file:\\.go$ http.Handle', description: 'Go modules using http.Handle' }
	];

	async function saveSearch(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (!user || !submitted) return;
		saveError = null;
		try {
			const r = await graphql<CreateSavedSearchMutation, CreateSavedSearchVariables>(
				CREATE_SAVED_SEARCH,
				{
					input: {
						owner: user.id,
						description: saveDescription.trim() || submitted.query,
						query: withPatternType(submitted.query, submitted.patternType),
						draft: false,
						visibility: 'SECRET'
					}
				}
			);
			savedURL = r.createSavedSearch.url;
			saving = false;
			saveDescription = '';
		} catch (err) {
			saveError = errorMessage(err);
		}
	}

	async function openSave(): Promise<void> {
		saving = true;
		saveDescription = '';
		saveError = null;
		await tick();
		document.getElementById('save-description')?.focus();
	}

	function isTyping(e: KeyboardEvent): boolean {
		const t = e.target as HTMLElement | null;
		return !!t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName));
	}

	/** `/` focuses the query box; j/k and the arrow keys move between results. */
	function onkeydown(e: KeyboardEvent): void {
		if (e.metaKey || e.ctrlKey || e.altKey || isTyping(e)) return;
		if (e.key === '/') {
			e.preventDefault();
			input?.focus();
			return;
		}
		const next = e.key === 'j' || e.key === 'ArrowDown';
		const prev = e.key === 'k' || e.key === 'ArrowUp';
		if (!(next || prev) || !resultsList) return;
		const headers = [...resultsList.querySelectorAll<HTMLElement>('.result-header')];
		if (headers.length === 0) return;
		const i = headers.findIndex((h) => h === document.activeElement || h.contains(document.activeElement));
		const j = i < 0 ? 0 : Math.min(Math.max(i + (next ? 1 : -1), 0), headers.length - 1);
		e.preventDefault();
		headers[j]?.focus();
		headers[j]?.scrollIntoView({ block: 'nearest' });
	}
</script>

<svelte:window {onkeydown} />

<svelte:head>
	<title>{submitted ? `${submitted.query} · Search` : 'Search'} · Sourcegull</title>
</svelte:head>

{#snippet searchBar(autofocus: boolean)}
	<form
		class="search-bar"
		role="search"
		onsubmit={(e) => {
			e.preventDefault();
			submit();
		}}
	>
		<ContextPicker value={context} onselect={selectContext} />
		<QueryInput
			bind:this={input}
			bind:value={text}
			{patternType}
			{autofocus}
			{suggest}
			placeholder="Search code… (repo:, file:, lang:, type:commit, …)"
			onsubmit={submit}
			ondiagnostics={(d) => (diagnostics = d)}
			recent={$recent}
			onselectrecent={selectRecent}
			onremoverecent={(q) => void recent.remove(q)}
			onclearrecent={() => void recent.clear()}
		/>
		<label class="visually-hidden" for="pattern-type">Pattern type</label>
		<select
			id="pattern-type"
			class="input pattern-type"
			bind:value={patternType}
			onchange={() => submitted && submit()}
		>
			{#each PATTERN_TYPES as t (t)}<option value={t}>{t}</option>{/each}
		</select>
		<button class="btn btn-primary submit" type="submit" aria-label="Search">
			<Icon name="search" size={16} /><span class="submit-label">Search</span>
		</button>
	</form>
	{#if diagnostics.length > 0}
		<p class="diagnostic" role="status">{diagnostics[0]?.message}</p>
	{/if}
{/snippet}

{#if !submitted}
	<div class="home">
		<div class="hero">
			<img class="hero-mark" src="/logo.png" alt="" width="88" height="64" />
			<h1>Search code</h1>
			<p class="hero-tagline">Find code, symbols, commits and diffs across your repositories.</p>
		</div>
		<div class="home-search">{@render searchBar(true)}</div>
		<p class="home-hint">
			Press <kbd>/</kbd> to focus the search box, <kbd>↑</kbd> for recent searches.
		</p>

		<section class="examples" aria-labelledby="examples-heading">
			<h2 id="examples-heading" class="section-label">Try a query</h2>
			<ul class="example-list">
				{#each EXAMPLES as ex (ex.query)}
					<li>
						<a class="query-chip" href={searchURL(ex.query)} title={ex.description}>
							<QueryText query={ex.query} />
						</a>
						<span class="example-desc">{ex.description}</span>
					</li>
				{/each}
			</ul>
		</section>

		{#if $recent.length > 0}
			<section class="card recent-panel" aria-labelledby="recent-searches-heading">
				<div class="card-header">
					<h2 id="recent-searches-heading"><Icon name="clock" size={15} /> Recent searches</h2>
					<button type="button" class="btn btn-ghost btn-sm" onclick={() => void recent.clear()}>Clear</button>
				</div>
				<div class="table-scroll">
					<table class="table">
						<thead>
							<tr><th scope="col">Search</th><th scope="col">Results</th><th scope="col">Date</th></tr>
						</thead>
						<tbody>
							{#each $recent.slice(0, 10) as e (e.query)}
								<tr>
									<td class="recent-panel-query"><a href={searchQueryURL(e.query)}><QueryText query={e.query} /></a></td>
									<td class="muted num">{resultCountLabel(e)}</td>
									<td class="muted" title={e.timestamp}>{relativeTime(e.timestamp)}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</section>
		{/if}

		<nav class="home-links" aria-label="Search tools">
			<a href="/saved-searches"><Icon name="bookmark" size={15} /> Saved searches</a>
			<a href="/contexts"><Icon name="context" size={15} /> Search contexts</a>
			<a href="/notebooks"><Icon name="notebook" size={15} /> Notebooks</a>
			<a href="/search-jobs"><Icon name="jobs" size={15} /> Search jobs</a>
		</nav>
	</div>
{:else}
	<div class="results-top">
		<div class="results-top-inner">{@render searchBar(false)}</div>
	</div>
	<div class="results-page">
		<aside class="side" id="search-filters" class:open={filtersOpen}>
			<FilterSidebar query={text} {filters} onchange={changeFilters} />
		</aside>
		<section class="main" aria-label="Search results">
			<div class="status toolbar">
				<button
					type="button"
					class="btn btn-sm filters-toggle"
					aria-expanded={filtersOpen}
					aria-controls="search-filters"
					onclick={() => (filtersOpen = !filtersOpen)}
				>
					<Icon name="filter" size={14} /> Filters
				</button>
				<span class="progress" role="status" aria-live="polite">
					{#if progress}
						<strong>{resultCount.toLocaleString()}{limitHit ? '+' : ''}</strong>
						result{resultCount === 1 && !limitHit ? '' : 's'}
						{#if progress.repositoriesCount != null}in <strong>{progress.repositoriesCount.toLocaleString()}</strong>
							repositor{progress.repositoriesCount === 1 ? 'y' : 'ies'}{/if}
						<span class="progress-time">· {seconds(progress.durationMs)}</span>
					{:else if running}
						Searching…
					{/if}
					{#if running}<span class="spinner" aria-hidden="true"></span>{/if}
				</span>
				<span class="spacer"></span>
				<div class="actions">
					<button type="button" class="btn btn-sm btn-ghost" aria-pressed={showAggregations} onclick={() => (showAggregations = !showAggregations)}>
						<Icon name="insights" size={14} /> Group by
					</button>
					{#if submitted}
						<a class="btn btn-sm btn-ghost" href={computePageURL(submitted.query)} title="Open this query in Compute.">
							<Icon name="compute" size={14} /> Compute
						</a>
					{/if}
					{#if user}
						<button type="button" class="btn btn-sm btn-ghost" onclick={openSave} disabled={saving}>
							<Icon name="bookmark" size={14} /> Save search
						</button>
						<button type="button" class="btn btn-sm btn-ghost" onclick={openJob} disabled={jobForm}>
							<Icon name="jobs" size={14} /> Create search job
						</button>
						{#if submitted}
							<a
								class="btn btn-sm btn-ghost"
								href={createInsightFromSearchURL(submitted.query, submitted.patternType)}
								title="Create a trend from this search query.">Create trend</a
							>
						{/if}
						{#if submitted && /(^|[\s(])type:(commit|diff)\b/i.test(submitted.query)}
							<a class="btn btn-sm btn-ghost" href={`/watches/new?q=${encodeURIComponent(withPatternType(submitted.query, submitted.patternType))}`}>
								<Icon name="monitor" size={14} /> Watch
							</a>
						{/if}
					{/if}
				</div>
			</div>

			{#if jobForm}
				<form class="save card" onsubmit={createJob} aria-label="Create search job">
					<p class="label">Create a search job</p>
					<code class="job-query">{jobQuery}</code>
					<p class="muted hint">
						A search job searches every repository and revision the query covers, as you, without the result limit and
						timeout of this page. Download the results from <a href="/search-jobs">search jobs</a> when it is done.
					</p>
					{#if jobInvalid}<div class="alert alert-warning" role="alert">{jobInvalid}</div>{/if}
					{#if jobError}<div class="alert alert-danger" role="alert">{jobError}</div>{/if}
					<div class="row">
						<button class="btn btn-primary btn-sm" type="submit" disabled={jobBusy || jobInvalid !== null}>
							Create search job
						</button>
						<button class="btn btn-sm" type="button" onclick={() => (jobForm = false)}>Cancel</button>
					</div>
				</form>
			{/if}

			{#if savedURL}
				<div class="alert alert-success" role="status">
					Saved. <a href={savedURL}>Open the saved search</a> or see all
					<a href="/saved-searches">saved searches</a>.
				</div>
			{/if}
			{#if saving}
				<form class="save card" onsubmit={saveSearch}>
					<label class="label" for="save-description">Description</label>
					<input id="save-description" class="input" bind:value={saveDescription} placeholder={submitted.query} />
					<p class="muted hint">Saved as secret (only you and site admins can see it). Change it in the library.</p>
					{#if saveError}<div class="alert alert-danger" role="alert">{saveError}</div>{/if}
					<div class="row">
						<button class="btn btn-primary btn-sm" type="submit">Save</button>
						<button class="btn btn-sm" type="button" onclick={() => (saving = false)}>Cancel</button>
					</div>
				</form>
			{/if}

			{#if skipped.length > 0}
				<details class="skipped" class:warn={incomplete}>
					<summary>
						<Icon name="chevron-right" size={14} class="skipped-chevron" />
						{skipped.length} note{skipped.length === 1 ? '' : 's'} about this search
						{#if incomplete}<span class="badge badge-warning">incomplete</span>{/if}
					</summary>
					<ul>
						{#each skipped as s, i (i)}
							<li><strong>{s.title}</strong>{#if s.message} — {s.message}{/if}</li>
						{/each}
					</ul>
				</details>
			{/if}

			{#each alerts as a, i (i)}
				<div class="alert alert-warning" role="alert">
					<strong>{a.title}</strong>
					{#if a.description}<p>{a.description}</p>{/if}
					{#if a.proposedQueries.length > 0}
						<ul class="proposed">
							{#each a.proposedQueries as pq (pq.query)}
								<li>
									<button type="button" class="query-chip" onclick={() => runQuery(pq.query)}>
										<QueryText query={pq.query} />
									</button>
									{#if pq.description}<span class="muted">{pq.description}</span>{/if}
								</li>
							{/each}
						</ul>
					{/if}
				</div>
			{/each}

			{#if streamError}
				<div class="alert alert-danger" role="alert">{streamError}</div>
			{/if}

			{#if showAggregations}
				<Aggregations query={submitted.query} patternType={submitted.patternType} onselect={runQuery} />
			{/if}

			<ol class="results" bind:this={resultsList}>
				{#each results.slice(0, rendered) as m, i (matchKey(m, i))}
					<li><SearchResult match={m} /></li>
				{/each}
			</ol>
			{#if rendered < results.length}
				<div bind:this={sentinel} class="sentinel loading">Loading more results…</div>
			{/if}
			{#if !running && results.length === 0 && !streamError}
				<div class="empty-state no-results">
					<span class="icon"><Icon name="search" size={22} /></span>
					<h3>No results</h3>
					<p class="muted">Try a broader query, another pattern type, or remove some filters.</p>
				</div>
			{/if}
		</section>
	</div>
{/if}

<style>
	/* ------------------------------------------------------------ home */
	.home {
		max-width: 68rem;
		margin: 0 auto;
		padding: clamp(var(--space-8), 10vh, 6rem) var(--space-4) var(--space-12);
		display: flex;
		flex-direction: column;
		gap: var(--space-6);
	}
	.hero {
		text-align: center;
		display: grid;
		justify-items: center;
		gap: var(--space-2);
	}
	.hero-mark {
		display: block;
		margin-bottom: var(--space-1);
	}
	.hero h1 {
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
		letter-spacing: -0.02em;
		margin: 0;
	}
	.hero-tagline {
		margin: 0;
		color: var(--color-text-muted);
	}
	.home-search {
		margin-top: var(--space-2);
	}
	.home-search :global(.search-bar) {
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-surface);
		box-shadow: var(--shadow-md);
	}
	.home-search :global(.query-input .cm-content) {
		padding-block: 0.7rem;
		font-size: var(--text-md);
	}
	.home-hint {
		margin: calc(-1 * var(--space-4)) 0 0;
		text-align: center;
		font-size: var(--text-xs);
		color: var(--color-text-subtle);
	}
	.section-label {
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--color-text-subtle);
		margin: 0 0 var(--space-2);
	}
	.example-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: var(--space-2);
	}
	.example-list li {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		min-width: 0;
	}
	.example-desc {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.query-chip {
		display: inline-flex;
		align-items: center;
		max-width: 100%;
		min-width: 0;
		overflow: hidden;
		padding: 0.2rem var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-bg-subtle);
		font-size: var(--text-sm);
		line-height: 1.4;
		cursor: pointer;
		flex: none;
		transition:
			border-color var(--duration) var(--ease),
			background-color var(--duration) var(--ease);
	}
	.query-chip:hover {
		text-decoration: none;
		border-color: var(--color-border-strong);
		background: var(--color-bg-hover);
	}
	.recent-panel {
		padding: 0;
		overflow: hidden;
	}
	.recent-panel .card-header {
		margin: 0;
		padding: var(--space-2) var(--space-3) var(--space-2) var(--space-4);
	}
	.recent-panel h2 {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: var(--text-md);
		font-weight: var(--weight-semibold);
		margin: 0;
	}
	.recent-panel h2 :global(.icon) {
		color: var(--color-text-muted);
	}
	.recent-panel .table {
		table-layout: fixed;
		margin: 0;
	}
	.recent-panel th:nth-child(2),
	.recent-panel th:nth-child(3) {
		width: 8.5rem;
	}
	.recent-panel td {
		white-space: nowrap;
	}
	.recent-panel .num {
		font-variant-numeric: tabular-nums;
	}
	.recent-panel-query {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.recent-panel-query a {
		font-size: var(--text-sm);
	}
	.recent-panel-query a:hover {
		text-decoration: none;
	}
	.recent-panel-query a:hover :global(.query-text) {
		text-decoration: underline;
	}
	.home-links {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: var(--space-2) var(--space-5);
		font-size: var(--text-sm);
	}
	.home-links a {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		color: var(--color-text-muted);
	}
	.home-links a:hover {
		color: var(--color-text);
	}
	@media (max-width: 40rem) {
		.recent-panel th:nth-child(2),
		.recent-panel td:nth-child(2),
		.example-desc {
			display: none;
		}
	}

	/* ------------------------------------------------------ search bar */
	.search-bar {
		display: flex;
		gap: var(--space-2);
		align-items: stretch;
	}
	.pattern-type {
		width: auto;
		flex: none;
	}
	.submit {
		flex: none;
		gap: var(--space-1);
	}
	.diagnostic {
		margin: var(--space-1) 0 0;
		color: var(--color-danger);
		font-size: var(--text-sm);
	}
	@media (max-width: 40rem) {
		.search-bar {
			flex-wrap: wrap;
		}
		.search-bar :global(.query-input) {
			order: -1;
			flex-basis: 100%;
		}
		.search-bar :global(.picker) {
			flex: 1;
			min-width: 0;
		}
		.submit-label {
			display: none;
		}
	}

	/* --------------------------------------------------------- results */
	.results-top {
		position: sticky;
		top: var(--nav-height);
		z-index: 15;
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg);
	}
	.results-top-inner {
		max-width: 110rem;
		margin: 0 auto;
		padding: var(--space-3) var(--space-4);
	}
	.results-page {
		display: grid;
		grid-template-columns: 15rem minmax(0, 1fr);
		grid-template-areas: 'side main';
		gap: var(--space-5);
		padding: var(--space-4);
		max-width: 110rem;
		margin: 0 auto;
	}
	.side {
		grid-area: side;
		position: sticky;
		/* below the sticky query bar (about one control plus padding) */
		top: calc(var(--nav-height) + 4.25rem);
		align-self: start;
		max-height: calc(100vh - var(--nav-height) - 5.5rem);
		overflow-y: auto;
		padding-right: var(--space-1);
	}
	.main {
		grid-area: main;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--space-3);
		align-content: start;
		min-width: 0;
	}
	.filters-toggle {
		display: none;
		gap: var(--space-1);
	}
	@media (max-width: 52rem) {
		.results-page {
			grid-template-columns: minmax(0, 1fr);
			grid-template-areas: 'side' 'main';
			gap: var(--space-3);
		}
		.side {
			display: none;
			position: static;
			max-height: none;
			padding: var(--space-3);
			border: 1px solid var(--color-border);
			border-radius: var(--radius-lg);
			background: var(--color-surface);
		}
		.side.open {
			display: block;
		}
		.filters-toggle {
			display: inline-flex;
		}
	}
	@media (max-width: 40rem) {
		.results-top-inner {
			padding: var(--space-2);
		}
		.results-page {
			padding: var(--space-3) var(--space-2);
		}
	}
	.status {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		margin: 0;
		min-height: var(--control-height-sm);
	}
	.progress {
		display: inline-flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 0 0.3em;
	}
	.progress strong {
		color: var(--color-text);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.progress-time {
		font-variant-numeric: tabular-nums;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.actions :global(.icon) {
		color: var(--color-text-muted);
	}
	.spacer {
		flex: 1;
	}
	.progress .spinner {
		margin-left: var(--space-2);
		color: var(--color-primary);
	}
	.results {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		min-width: 0;
		gap: var(--space-3);
	}
	.results > li {
		min-width: 0;
	}
	.skipped {
		font-size: var(--text-sm);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-bg-subtle);
	}
	.skipped.warn {
		border-color: var(--color-warning-border);
		background: var(--color-warning-bg);
	}
	.skipped summary {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		cursor: pointer;
		list-style: none;
	}
	.skipped summary::-webkit-details-marker {
		display: none;
	}
	.skipped :global(.skipped-chevron) {
		transition: transform var(--duration) var(--ease);
	}
	.skipped[open] :global(.skipped-chevron) {
		transform: rotate(90deg);
	}
	.skipped ul {
		margin: 0;
		padding: 0 var(--space-3) var(--space-3) calc(var(--space-3) + 1.4rem);
		display: grid;
		gap: var(--space-1);
	}
	.save {
		padding: var(--space-3);
		display: grid;
		gap: var(--space-2);
	}
	.job-query {
		overflow-wrap: anywhere;
	}
	.save .hint {
		margin: 0;
		font-size: var(--text-sm);
	}
	.row {
		display: flex;
		gap: var(--space-2);
	}
	.alert ul {
		margin: var(--space-1) 0 0;
	}
	.alert p {
		margin: var(--space-1) 0 0;
	}
	.proposed {
		list-style: none;
		padding: 0;
		display: grid;
		gap: var(--space-1);
	}
	.proposed li {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
	}
	.proposed .query-chip {
		background: var(--color-surface);
		font: inherit;
		color: inherit;
	}
	.sentinel {
		justify-content: center;
		padding: var(--space-4);
	}
	.no-results {
		margin-top: var(--space-2);
	}
</style>
