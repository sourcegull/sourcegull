<script lang="ts">
	import { repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		urlRev: string | null;
		path: string;
		/** Whether the last segment is a file. */
		isFile: boolean;
	}

	let { repo, urlRev, path, isFile }: Props = $props();

	const segments = $derived.by(() => {
		const parts = path.split('/').filter(Boolean);
		return parts.map((name, i) => {
			const p = parts.slice(0, i + 1).join('/');
			const last = i === parts.length - 1;
			return { name, last, href: repoPageURL(repo, urlRev, last && isFile ? 'blob' : 'tree', p) };
		});
	});
</script>

<nav class="crumbs mono" aria-label="Path">
	<a href={repoPageURL(repo, urlRev, 'root')}>{repo.split('/').pop()}</a>
	{#each segments as s (s.href)}
		<span class="sep" aria-hidden="true">/</span>
		{#if s.last}
			<span aria-current="page">{s.name}</span>
		{:else}
			<a href={s.href}>{s.name}</a>
		{/if}
	{/each}
</nav>

<style>
	.crumbs {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.2rem;
		min-width: 0;
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
	}
	.crumbs a {
		color: var(--color-text-muted);
		border-radius: var(--radius-sm);
	}
	.crumbs a:hover {
		color: var(--color-link);
	}
	.sep {
		color: var(--color-text-subtle);
	}
	[aria-current='page'] {
		color: var(--color-text);
		font-weight: var(--weight-semibold);
	}
</style>
