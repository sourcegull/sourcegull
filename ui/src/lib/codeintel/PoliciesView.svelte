<!--
	Configuration policies: list, create, edit and delete (site admins). Policies the resources file
	owns are read-only; when it owns every policy not tied to one repository, the site-wide list
	offers no "New policy".
-->
<script lang="ts">
	import { loadAuthState } from '#lib/auth.ts';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { DECLARED_KINDS, isExclusive, type DeclaredKindsResult } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_POLICY,
		DELETE_POLICY,
		describeRetention,
		describeScope,
		describeTarget,
		formatHours,
		parsePatterns,
		POLICIES,
		PREVIEW_GIT_OBJECTS,
		PREVIEW_REPOSITORIES,
		splitHours,
		UNITS,
		UPDATE_POLICY,
		type CreatePolicyVariables,
		type PoliciesResult,
		type PoliciesVariables,
		type PolicyInput,
		type PolicyNode,
		policyType,
		type PolicyType,
		type PreviewGitObjectsResult,
		type PreviewGitObjectsVariables,
		type PreviewRepositoriesResult,
		type PreviewRepositoriesVariables,
		type UpdatePolicyVariables
	} from './policies.ts';

	interface Props {
		/** The repository's node ID: list the policies that apply to it, and create policies for it. */
		repoId?: string;
	}

	let { repoId }: Props = $props();

	interface Draft {
		id: string | null;
		protected: boolean;
		name: string;
		type: PolicyType;
		pattern: string;
		patterns: string;
		retentionEnabled: boolean;
		retentionAmount: number | null;
		retentionUnit: number;
		retainIntermediateCommits: boolean;
		indexingEnabled: boolean;
		indexAmount: number | null;
		indexUnit: number;
		indexIntermediateCommits: boolean;
	}

	let nodes = $state.raw<PolicyNode[]>([]);
	let total = $state(0);
	let endCursor = $state<string | null>(null);
	let query = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let siteAdmin = $state(false);
	/** Whether the resources file owns every policy not tied to one repository. */
	let exclusive = $state(false);
	let draft = $state<Draft | null>(null);
	let saving = $state(false);
	let formError = $state<string | null>(null);
	let preview = $state<string | null>(null);
	let previewItems = $state.raw<{ label: string; href?: string }[]>([]);
	let seq = 0;

	loadAuthState()
		.then(async (s) => {
			siteAdmin = s.user?.siteAdmin === true;
			if (siteAdmin && !repoId) {
				const r = await graphql<DeclaredKindsResult>(DECLARED_KINDS);
				exclusive = isExclusive(r.site.declaredConfigFiles, 'codeIntelPolicies');
			}
		})
		.catch(() => {});

	$effect(() => {
		void load(repoId, false);
	});

	async function load(repo: string | undefined, more: boolean): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<PoliciesResult, PoliciesVariables>(POLICIES, {
				repo: repo ?? null,
				query: query.trim() || null,
				first: 50,
				after: more ? endCursor : null
			});
			if (mine !== seq) return;
			const c = r.codeIntelligenceConfigurationPolicies;
			nodes = more ? [...nodes, ...c.nodes] : c.nodes;
			total = c.totalCount ?? nodes.length;
			endCursor = c.pageInfo.hasNextPage ? c.pageInfo.endCursor : null;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(repoId, false), 200);
	}

	function edit(p: PolicyNode | null): void {
		formError = null;
		const retention = splitHours(p?.retentionDurationHours ?? null);
		const index = splitHours(p?.indexCommitMaxAgeHours ?? null);
		draft = {
			id: p?.id ?? null,
			protected: p?.protected ?? false,
			name: p?.name ?? '',
			type: p ? policyType(p.type) : 'GIT_TREE',
			pattern: p?.pattern ?? '*',
			patterns: p?.repositoryPatterns?.join('\n') ?? '',
			retentionEnabled: p?.retentionEnabled ?? true,
			retentionAmount: p ? retention.amount : 4,
			retentionUnit: p ? retention.unit : 24 * 7,
			retainIntermediateCommits: p?.retainIntermediateCommits ?? false,
			indexingEnabled: p?.indexingEnabled ?? false,
			indexAmount: index.amount,
			indexUnit: index.unit,
			indexIntermediateCommits: p?.indexIntermediateCommits ?? false
		};
		void refreshPreview();
	}

	function hours(amount: number | null, unit: number): number | null {
		return amount === null || Number.isNaN(amount) ? null : Math.round(amount * unit);
	}

	async function save(): Promise<void> {
		if (!draft) return;
		saving = true;
		formError = null;
		const d = draft;
		const vars: PolicyInput = {
			repositoryPatterns: repoId ? null : parsePatterns(d.patterns),
			name: d.name,
			type: d.type,
			pattern: d.pattern,
			retentionEnabled: d.retentionEnabled,
			retentionDurationHours: d.retentionEnabled ? hours(d.retentionAmount, d.retentionUnit) : null,
			retainIntermediateCommits: d.type === 'GIT_TREE' && d.retainIntermediateCommits,
			indexingEnabled: d.indexingEnabled,
			indexCommitMaxAgeHours: d.indexingEnabled ? hours(d.indexAmount, d.indexUnit) : null,
			indexIntermediateCommits: d.type === 'GIT_TREE' && d.indexIntermediateCommits
		};
		try {
			if (d.id) {
				await graphql<unknown, UpdatePolicyVariables>(UPDATE_POLICY, { id: d.id, ...vars });
			} else {
				await graphql<unknown, CreatePolicyVariables>(CREATE_POLICY, { repository: repoId ?? null, ...vars });
			}
			draft = null;
			await load(repoId, false);
		} catch (err) {
			formError = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	async function remove(p: PolicyNode): Promise<void> {
		if (!confirm(`Delete the policy "${p.name}"? Indexes it keeps may expire.`)) return;
		error = null;
		try {
			await graphql<unknown, { id: string }>(DELETE_POLICY, { id: p.id });
			await load(repoId, false);
		} catch (err) {
			error = errorMessage(err);
		}
	}

	let previewSeq = 0;
	let previewTimer: ReturnType<typeof setTimeout> | undefined;
	function schedulePreview(): void {
		clearTimeout(previewTimer);
		previewTimer = setTimeout(() => void refreshPreview(), 250);
	}

	/** What the draft names: git objects of this repository, or (site-wide) the repositories its patterns match. */
	async function refreshPreview(): Promise<void> {
		const d = draft;
		const mine = ++previewSeq;
		if (!d) return;
		try {
			if (repoId) {
				if (!d.pattern.trim()) {
					preview = null;
					previewItems = [];
					return;
				}
				const r = await graphql<PreviewGitObjectsResult, PreviewGitObjectsVariables>(PREVIEW_GIT_OBJECTS, {
					repo: repoId,
					type: d.type,
					pattern: d.pattern
				});
				if (mine !== previewSeq) return;
				const f = r.node?.previewGitObjectFilter;
				const n = f?.totalCount ?? 0;
				const what = d.type === 'GIT_TREE' ? 'branch' : d.type === 'GIT_TAG' ? 'tag' : 'commit';
				const plural = what === 'branch' ? 'branches' : `${what}s`;
				preview = n === 0 ? `Matches no ${what} of this repository.` : `Matches ${n} ${n === 1 ? what : plural}:`;
				previewItems = (f?.nodes ?? []).map((o) => ({ label: `${o.name}${o.name === o.rev ? '' : ` (${o.rev.slice(0, 7)})`}` }));
			} else {
				const patterns = parsePatterns(d.patterns);
				if (!patterns || !siteAdmin) {
					preview = patterns ? null : 'Applies to all repositories.';
					previewItems = [];
					return;
				}
				const r = await graphql<PreviewRepositoriesResult, PreviewRepositoriesVariables>(PREVIEW_REPOSITORIES, {
					patterns
				});
				if (mine !== previewSeq) return;
				const f = r.previewRepositoryFilter;
				preview = f.matchesAllRepos
					? 'Matches every repository.'
					: `Matches ${f.totalMatches} ${f.totalMatches === 1 ? 'repository' : 'repositories'}${f.totalMatches ? ':' : '.'}`;
				previewItems = f.nodes.map((n) => ({ label: n.name, href: n.url }));
			}
		} catch (err) {
			if (mine === previewSeq) {
				preview = errorMessage(err);
				previewItems = [];
			}
		}
	}
</script>

<div class="policies">
	<div class="page-header">
		<h2>Configuration policies</h2>
		<span class="muted">{total} {total === 1 ? 'policy' : 'policies'}</span>
		{#if siteAdmin && !draft && !exclusive}
			<button class="btn btn-primary btn-sm new" onclick={() => edit(null)}>
				{repoId ? 'New policy for this repository' : 'New policy'}
			</button>
		{/if}
	</div>
	<p class="muted intro">
		Policies name branches, tags or commits. Precise indexes that serve them are kept for the policy's duration;
		older ones expire and are deleted. The tip of the default branch always keeps the indexes it uses.
	</p>
	{#if exclusive}<ManagedNotice kind="codeIntelPolicies" />{/if}

	{#if draft}
		<form class="editor" onsubmit={(e) => { e.preventDefault(); void save(); }}>
			<h3>{draft.id ? `Edit ${draft.protected ? 'built-in ' : ''}policy` : 'New policy'}</h3>
			{#if draft.protected}
				<p class="muted">Built-in policies keep their name and what they match; durations and switches can change.</p>
			{/if}
			<label class="field">
				<span>Name</span>
				<input class="input" bind:value={draft.name} required disabled={draft.protected} />
			</label>
			{#if !repoId}
				<label class="field">
					<span>Repositories <span class="muted">(globs, one per line; empty for all)</span></span>
					<textarea
						class="input mono"
						rows="2"
						bind:value={draft.patterns}
						oninput={schedulePreview}
						disabled={draft.protected}
						placeholder="gitlab.example.com/team/*"
					></textarea>
				</label>
			{/if}
			<div class="row">
				<label class="field">
					<span>Type</span>
					<select class="input" bind:value={draft.type} onchange={schedulePreview} disabled={draft.protected}>
						<option value="GIT_TREE">Branches</option>
						<option value="GIT_TAG">Tags</option>
						<option value="GIT_COMMIT">Commit</option>
					</select>
				</label>
				<label class="field grow">
					<span>{draft.type === 'GIT_COMMIT' ? 'Commit' : 'Name pattern (glob)'}</span>
					<input class="input mono" bind:value={draft.pattern} oninput={schedulePreview} required disabled={draft.protected} />
				</label>
			</div>
			{#if preview}
				<div class="preview" role="status">
					<span class="muted">{preview}</span>
					{#if previewItems.length}
						<ul>
							{#each previewItems as item (item.label)}
								<li class="mono">{#if item.href}<a href={item.href}>{item.label}</a>{:else}{item.label}{/if}</li>
							{/each}
						</ul>
					{/if}
				</div>
			{/if}

			<fieldset>
				<legend><label><input type="checkbox" bind:checked={draft.retentionEnabled} /> Keep precise indexes</label></legend>
				{#if draft.retentionEnabled}
					<div class="row">
						<label class="field">
							<span>For</span>
							<input class="input amount" type="number" min="1" placeholder="forever" bind:value={draft.retentionAmount} />
						</label>
						<label class="field">
							<span><span class="visually-hidden">Unit</span>&nbsp;</span>
							<select class="input" bind:value={draft.retentionUnit}>
								{#each UNITS as u (u.hours)}<option value={u.hours}>{u.label}</option>{/each}
							</select>
						</label>
					</div>
					<p class="muted hint">After upload. Leave empty to keep them forever.</p>
					{#if draft.type === 'GIT_TREE'}
						<label class="check">
							<input type="checkbox" bind:checked={draft.retainIntermediateCommits} disabled={draft.protected} />
							Also for every commit on matching branches, not only their tips
						</label>
					{/if}
				{/if}
			</fieldset>

			<fieldset>
				<legend><label><input type="checkbox" bind:checked={draft.indexingEnabled} /> Auto-index</label></legend>
				{#if draft.indexingEnabled}
					<div class="row">
						<label class="field">
							<span>Commits newer than</span>
							<input class="input amount" type="number" min="1" placeholder="any age" bind:value={draft.indexAmount} />
						</label>
						<label class="field">
							<span><span class="visually-hidden">Unit</span>&nbsp;</span>
							<select class="input" bind:value={draft.indexUnit}>
								{#each UNITS as u (u.hours)}<option value={u.hours}>{u.label}</option>{/each}
							</select>
						</label>
					</div>
					{#if draft.type === 'GIT_TREE'}
						<label class="check">
							<input type="checkbox" bind:checked={draft.indexIntermediateCommits} disabled={draft.protected} />
							Index every commit on matching branches, not only their tips
						</label>
					{/if}
					<p class="muted hint">Auto-indexing runs on executors, which are not set up yet; the setting is kept for them.</p>
				{/if}
			</fieldset>

			{#if formError}
				<div class="alert alert-danger" role="alert">{formError}</div>
			{/if}
			<div class="buttons">
				<button class="btn btn-primary" type="submit" disabled={saving}>{draft.id ? 'Save' : 'Create policy'}</button>
				<button class="btn" type="button" onclick={() => (draft = null)} disabled={saving}>Cancel</button>
			</div>
		</form>
	{/if}

	<div class="filters">
		<label class="visually-hidden" for="policy-filter">Filter policies</label>
		<input id="policy-filter" class="input" type="search" placeholder="Filter by name…" bind:value={query} {oninput} />
	</div>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}

	<div class="table-scroll">
		<table class="table" aria-busy={loading}>
			<thead>
				<tr>
					<th>Name</th>
					<th>Repositories</th>
					<th>Matches</th>
					<th>Retention</th>
					<th>Auto-indexing</th>
					{#if siteAdmin}<th><span class="visually-hidden">Actions</span></th>{/if}
				</tr>
			</thead>
			<tbody>
				{#each nodes as p (p.id)}
					<tr data-policy={p.name}>
						<td>
							{p.name}
							{#if p.protected}<span class="badge" title="Built in: can be edited, not deleted">built-in</span>{/if}
							<ManagedBadge managedBy={p.managedBy} />
						</td>
						<td class="scope">
							{#if p.repository}<a href={p.repository.url}>{p.repository.name}</a>{:else}{describeScope(p)}{/if}
						</td>
						<td>{describeTarget(p.type, p.pattern)}</td>
						<td>{describeRetention(p) || '—'}</td>
						<td>
							{#if p.indexingEnabled}
								{p.indexCommitMaxAgeHours === null ? 'Any age' : `Commits from the last ${formatHours(p.indexCommitMaxAgeHours)}`}
							{:else}—{/if}
						</td>
						{#if siteAdmin}
							<td class="actions nowrap">
								{#if !p.managedBy}
									<button class="btn btn-sm" onclick={() => edit(p)}>Edit</button>
								{/if}
								{#if !p.protected && !p.managedBy}
									<button class="btn btn-sm btn-danger" onclick={() => remove(p)}>Delete</button>
								{/if}
							</td>
						{/if}
					</tr>
				{:else}
					<tr><td colspan={siteAdmin ? 6 : 5} class="muted">{loading ? 'Loading…' : 'No policies.'}</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
	{#if endCursor}
		<button class="btn more" disabled={loading} onclick={() => load(repoId, true)}>Show more</button>
	{/if}
</div>

<style>
	.policies {
		max-width: 80rem;
	}
	.page-header .new {
		margin-left: auto;
	}
	.intro {
		margin-top: 0;
	}
	.filters {
		display: flex;
		margin-bottom: var(--space-3);
	}
	.filters input {
		flex: 1 1 16rem;
		max-width: 32rem;
	}
	.editor {
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-3);
		margin-bottom: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		max-width: 44rem;
	}
	.editor h3 {
		margin: 0;
	}
	.editor p {
		margin: 0;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		align-items: flex-end;
	}
	.grow {
		flex: 1 1 14rem;
	}
	.amount {
		width: 8rem;
	}
	fieldset {
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-2) var(--space-3) var(--space-3);
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
	}
	legend label,
	.check {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.hint {
		font-size: 0.85em;
	}
	.preview ul {
		margin: var(--space-1) 0 0;
		padding-left: var(--space-4);
		font-size: 0.9em;
	}
	.buttons {
		display: flex;
		gap: var(--space-2);
	}
	.scope {
		overflow-wrap: anywhere;
	}
	.badge {
		margin-left: var(--space-1);
	}
	.nowrap {
		white-space: nowrap;
	}
	.actions {
		text-align: right;
	}
	.actions .btn + .btn {
		margin-left: var(--space-1);
	}
	.more {
		margin-top: var(--space-3);
	}
</style>
