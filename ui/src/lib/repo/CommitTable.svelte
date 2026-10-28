<script lang="ts">
	import { relativeDate } from './blame.ts';
	import type { CommitSummary } from './queries.ts';
	import { repoURL } from './url.ts';

	interface Props {
		repo: string;
		commits: CommitSummary[];
		emptyText?: string;
	}

	let { repo, commits, emptyText = 'No commits.' }: Props = $props();
</script>

<table class="table commits">
	<tbody>
		{#each commits as c (c.oid)}
			<tr>
				<td class="subject"><a href={c.url}>{c.subject}</a></td>
				<td class="author muted" title={c.author.person.email}>{c.author.person.name}</td>
				<td class="date muted" title={c.author.date}>{relativeDate(c.author.date)}</td>
				<td class="oid">
					<a class="mono" href={c.url}>{c.abbreviatedOID}</a>
					<a class="muted browse" href={repoURL(repo, c.oid)} title="Browse files at this commit">files</a>
				</td>
			</tr>
		{:else}
			<tr><td class="muted">{emptyText}</td></tr>
		{/each}
	</tbody>
</table>

<style>
	.subject {
		width: 60%;
		overflow-wrap: anywhere;
	}
	.author,
	.date {
		white-space: nowrap;
	}
	.oid {
		white-space: nowrap;
		text-align: right;
	}
	.browse {
		margin-left: var(--space-2);
		font-size: 0.9em;
	}
</style>
