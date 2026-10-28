<script lang="ts">
	import { errorMessage } from '#lib/graphql.ts';
	import {
		assign,
		coverage,
		isSignal,
		loadOwnership,
		ownerName,
		reasonLabel,
		unassign,
		type LoadedOwnership,
		type Ownership,
		type OwnershipReason
	} from './queries.ts';

	/** The owners of a file or directory with their reasons (Code Ownership). */
	interface Props {
		repo: string;
		/** The revision (defaults to HEAD). */
		rev: string | null;
		path: string;
		isDir: boolean;
	}

	let { repo, rev, path, isDir }: Props = $props();

	let data = $state.raw<LoadedOwnership | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let who = $state('');
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let seq = 0;

	$effect(() => {
		void load(repo, rev ?? 'HEAD', path, isDir);
	});

	async function load(r: string, v: string, p: string, dir: boolean): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const d = await loadOwnership(r, v, p, dir);
			if (mine === seq) data = d;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	const owners = $derived(data?.ownership?.nodes ?? []);
	const stats = $derived(data?.stats?.updatedAt ? data.stats : null);

	function direct(o: Ownership): boolean {
		return o.reasons.some((r) => r.__typename === 'AssignedOwner' && r.isDirectMatch);
	}

	function reasonURL(r: OwnershipReason): string | null {
		if (r.__typename !== 'CodeownersFileEntry') return null;
		const f = r.codeownersFile;
		return f.__typename === 'GitBlob' ? `${f.url}#L${r.ruleLineMatch}` : f.url;
	}

	function ownerURL(o: Ownership['owner']): string | null {
		if (o.__typename === 'Team') return o.external ? null : o.url;
		return o.user?.url ?? null;
	}

	async function run(action: () => Promise<string | null | void>): Promise<void> {
		busy = true;
		actionError = null;
		try {
			const msg = await action();
			if (msg) {
				actionError = msg;
				return;
			}
			who = '';
			await load(repo, rev ?? 'HEAD', path, isDir);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function onassign(e: SubmitEvent): void {
		e.preventDefault();
		const repoID = data?.repoID;
		if (!repoID) return;
		void run(() => assign(who, repoID, path));
	}
</script>

<section class="ownership" aria-label="Owners">
	<header>
		<h3>Owners</h3>
		{#if data?.ownership}<span class="muted">{data.ownership.totalOwners}</span>{/if}
	</header>
	{#if stats && stats.totalFiles > 0}
		<p class="stats muted" title="Default branch, computed in the background">
			{stats.totalOwnedFiles} of {stats.totalFiles} files owned ({coverage(stats.totalOwnedFiles, stats.totalFiles)}%):
			{stats.totalCodeownedFiles} by CODEOWNERS, {stats.totalAssignedOwnershipFiles} assigned
		</p>
	{/if}
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading && !data}
		<p class="muted">Loading…</p>
	{:else if owners.length === 0}
		<p class="muted">
			No owners. Add a <span class="mono">CODEOWNERS</span> file (in the root, <span class="mono">docs/</span>
			or <span class="mono">.gitlab/</span>){data?.canAssign ? ' or assign an owner below' : ''}.
		</p>
	{:else}
		<ul class="owners">
			{#each owners as o, i (i)}
				{@const url = ownerURL(o.owner)}
				<li>
					<span class="avatar" aria-hidden="true">
						{#if o.owner.__typename === 'Person' && o.owner.avatarURL}
							<img src={o.owner.avatarURL} alt="" />
						{:else}
							{ownerName(o.owner).replace(/^@/, '').slice(0, 1).toUpperCase()}
						{/if}
					</span>
					<span class="who">
						{#if url}<a href={url}>{ownerName(o.owner)}</a>{:else}{ownerName(o.owner)}{/if}
						{#if o.owner.__typename === 'Team'}
							<span class="badge">{o.owner.external ? 'GitLab group' : 'team'}</span>
						{:else if !o.owner.user}
							<span class="badge" title="No user matches this handle or email">not a user</span>
						{/if}
					</span>
					<span class="reasons">
						{#each o.reasons as r, j (j)}
							{@const href = reasonURL(r)}
							{#if href}
								<a class="reason" {href} title={r.__typename === 'CodeownersFileEntry' ? r.pattern : ''}>{reasonLabel(r)}</a>
							{:else}
								<span class="reason" class:signal={isSignal(r)}>{reasonLabel(r)}</span>
							{/if}
						{/each}
					</span>
					{#if data?.canAssign && direct(o)}
						<button
							type="button"
							class="btn btn-sm"
							disabled={busy}
							onclick={() => {
								const repoID = data?.repoID;
								if (repoID) void run(() => unassign(o.owner, repoID, path));
							}}>Remove</button
						>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
	{#if data?.canAssign}
		<form class="assign" onsubmit={onassign}>
			<input
				class="input"
				type="text"
				placeholder="username or team:name"
				aria-label="Owner to assign"
				bind:value={who}
				disabled={busy}
			/>
			<button type="submit" class="btn btn-sm" disabled={busy || !who.trim()}>
				Assign to this {isDir ? (path === '' ? 'repository' : 'directory') : 'file'}
			</button>
		</form>
		{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
	{/if}
</section>

<style>
	.ownership {
		padding: var(--space-3) var(--space-4);
		border-bottom: 1px solid var(--color-border);
	}
	header {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
	}
	h3 {
		margin: 0 0 var(--space-2);
		font-size: 1em;
	}
	.owners {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.owners li {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		padding: var(--space-1) 0;
	}
	.avatar {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.5rem;
		height: 1.5rem;
		border-radius: 50%;
		background: var(--color-bg-muted);
		font-size: 0.8em;
		overflow: hidden;
		flex: none;
	}
	.avatar img {
		width: 100%;
		height: 100%;
	}
	.who {
		font-weight: 600;
	}
	.reasons {
		display: inline-flex;
		flex-wrap: wrap;
		gap: var(--space-1);
		flex: 1;
	}
	.reason {
		font-size: 0.85em;
		padding: 0 var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: 999px;
		white-space: nowrap;
	}
	.reason.signal {
		border-style: dashed;
		color: var(--color-text-muted);
	}
	.stats {
		margin: 0 0 var(--space-2);
		font-size: 0.85em;
	}
	.assign {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-top: var(--space-2);
	}
	.assign .input {
		flex: 1;
		min-width: 12rem;
	}
	.alert {
		margin: var(--space-2) 0 0;
	}
</style>
