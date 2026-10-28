/**
 * Links to a repository, file or commit on its code host ("View on GitLab", "View on GitHub",
 * "View on Gitea").
 *
 * The code host URL never comes from the repository name (a repo named `gitlab.example/x/y` may
 * live at `https://gitlab.internal/x/y`): it is the server's stored external URL for the repo
 * (GraphQL `Repository.externalURLs`, the project's `web_url` from GitLab). Search results carry
 * only repo names, so {@link ExternalURLLoader} fetches the URLs of the visible repos in batched
 * GraphQL requests; `repository(name:)` is permission-checked, so only repos the viewer can see
 * get a link.
 */
import { SvelteMap } from 'svelte/reactivity';
import { graphql } from '#lib/graphql.ts';

export interface ExternalLink {
	url: string;
	serviceKind: string | null;
}

/** What to link to on the code host. */
export type CodeHostTarget =
	| { kind: 'repo' }
	| { kind: 'blob' | 'tree'; rev: string; path: string; line?: number }
	| { kind: 'commit'; oid: string };

function encodePath(p: string): string {
	return p
		.split('/')
		.filter((s) => s !== '')
		.map(encodeURIComponent)
		.join('/');
}

/** The path segments of a code host's file, directory and commit pages, after the repo URL. */
interface DeepLinks {
	blob: string;
	tree: string;
	commit: string;
}

const DEEP_LINKS: Record<string, DeepLinks> = {
	GITLAB: { blob: '-/blob', tree: '-/tree', commit: '-/commit' },
	GITHUB: { blob: 'blob', tree: 'tree', commit: 'commit' },
	// Gitea redirects `src/<rev>` to the branch, tag or commit page the revision names.
	GITEA: { blob: 'src', tree: 'src', commit: 'commit' }
};

/**
 * The code host URL of `target` in the repository at `link`. GitLab, GitHub and Gitea get deep
 * links (GitLab `/-/blob/<rev>/<path>#L<n>`, `/-/tree/…`, `/-/commit/<oid>`; GitHub the same
 * without `/-`; Gitea `/src/<rev>/<path>` for files and directories); other hosts get the repo
 * page.
 */
export function codeHostURL(link: ExternalLink, target: CodeHostTarget): string {
	const base = link.url.replace(/\/+$/, '');
	const links = link.serviceKind ? DEEP_LINKS[link.serviceKind] : undefined;
	if (!links || target.kind === 'repo') return base;
	if (target.kind === 'commit') return `${base}/${links.commit}/${encodeURIComponent(target.oid)}`;
	const rev = encodePath(target.rev) || 'HEAD';
	const path = encodePath(target.path);
	const url = `${base}/${links[target.kind]}/${rev}${path ? `/${path}` : ''}`;
	return target.kind === 'blob' && target.line ? `${url}#L${target.line}` : url;
}

/** "GitLab", "GitHub" or "Gitea" for their links, else "code host" (for "View on …" labels). */
export function codeHostName(link: ExternalLink): string {
	switch (link.serviceKind) {
		case 'GITLAB':
			return 'GitLab';
		case 'GITHUB':
			return 'GitHub';
		case 'GITEA':
			return 'Gitea';
		default:
			return 'code host';
	}
}

/** One request resolves at most this many repositories. */
export const BATCH_SIZE = 50;

/** A query resolving `count` repository names (variables `$r0`…) to their external URLs. */
export function externalURLsQuery(count: number): string {
	const vars = Array.from({ length: count }, (_, i) => `$r${i}: String!`).join(', ');
	const fields = Array.from(
		{ length: count },
		(_, i) => `r${i}: repository(name: $r${i}) { externalURLs { url serviceKind } }`
	).join('\n\t');
	return `query RepositoryExternalURLs(${vars}) {\n\t${fields}\n}`;
}

type Fetcher = (
	query: string,
	variables: Record<string, string>
) => Promise<Record<string, { externalURLs: ExternalLink[] } | null>>;

/**
 * Resolves repository names to their first external link, batching the names requested in one
 * tick into requests of {@link BATCH_SIZE}. Results are cached for the page's lifetime; a repo
 * the viewer cannot see, one without a URL or a failed request caches `null` (no link, no retry).
 */
export class ExternalURLLoader {
	readonly #links = new SvelteMap<string, ExternalLink | null>();
	readonly #requested = new Set<string>();
	#pending: string[] = [];
	#scheduled = false;

	constructor(
		private readonly fetcher: Fetcher = (q, v) => graphql(q, v),
		private readonly schedule: (f: () => void) => void = (f) => setTimeout(f, 0)
	) {}

	/** The repo's link: undefined while unknown (and requests it), null when there is none. */
	get(name: string): ExternalLink | null | undefined {
		const link = this.#links.get(name);
		if (link === undefined) this.request(name);
		return link;
	}

	request(name: string): void {
		if (this.#requested.has(name)) return;
		this.#requested.add(name);
		this.#pending.push(name);
		if (!this.#scheduled) {
			this.#scheduled = true;
			this.schedule(() => void this.flush());
		}
	}

	/** Sends the pending names (normally called by the scheduler). */
	async flush(): Promise<void> {
		this.#scheduled = false;
		const names = this.#pending;
		this.#pending = [];
		const batches: string[][] = [];
		for (let i = 0; i < names.length; i += BATCH_SIZE) batches.push(names.slice(i, i + BATCH_SIZE));
		await Promise.all(batches.map((b) => this.#load(b)));
	}

	async #load(names: string[]): Promise<void> {
		const variables = Object.fromEntries(names.map((n, i) => [`r${i}`, n]));
		let data: Awaited<ReturnType<Fetcher>> = {};
		try {
			data = await this.fetcher(externalURLsQuery(names.length), variables);
		} catch {
			// No links rather than an error banner: the results themselves are fine.
		}
		names.forEach((n, i) => {
			this.#links.set(n, data[`r${i}`]?.externalURLs[0] ?? null);
		});
	}
}

/** The loader shared by every search result on the page. */
export const externalURLs = new ExternalURLLoader();
