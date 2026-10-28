/**
 * URL conventions of repository pages (mirrors `sourcegull_services::git::RepoUrl`):
 *
 *   /{repo}[@{rev}]                      repository root (file tree at rev)
 *   /{repo}[@{rev}]/-/tree/{path}        directory
 *   /{repo}[@{rev}]/-/blob/{path}        file (line range in the hash: #L10 or #L10-20)
 *   /{repo}/-/commit/{oid}               commit with its diff
 *   /{repo}[@{rev}]/-/commits[/{path}]   history
 *   /{repo}/-/compare/{base}...{head}    comparison
 *   /{repo}/-/branches, /-/tags          refs
 *   /{repo}/-/code-graph                 code graph data (precise indexes)
 *   /{repo}/-/settings                   repository settings (metadata)
 *   /{repo}[@{rev}]/-/raw/{path}         raw file or archive (served by the backend)
 *
 * Repository names never contain a `-` path segment or `@`, so the first `/-/` separates the
 * repository (and revision) from the page, and the first `@` separates the revision.
 */

export type RepoPageKind =
	| 'root'
	| 'tree'
	| 'blob'
	| 'commit'
	| 'commits'
	| 'compare'
	| 'branches'
	| 'tags'
	| 'code-graph'
	| 'settings';

const PAGE_KINDS: ReadonlySet<string> = new Set([
	'tree',
	'blob',
	'commit',
	'commits',
	'compare',
	'branches',
	'tags',
	'code-graph',
	'settings'
]);

export interface RepoRoute {
	/** Repository name, e.g. `gitlab.com/group/project`. */
	repo: string;
	/** Revision after `@`, or null for the default branch. */
	rev: string | null;
	kind: RepoPageKind;
	/** File/directory path (tree, blob, commits), commit oid (commit) or range (compare). */
	path: string;
}

function decode(s: string): string {
	try {
		return decodeURIComponent(s);
	} catch {
		return s;
	}
}

/** Parses a URL pathname into a repository route; null if it cannot be a repository page. */
export function parseRepoRoute(pathname: string): RepoRoute | null {
	let p = pathname.replace(/^\/+/, '');
	if (p === '') return null;
	let page = '';
	const sep = p.indexOf('/-/');
	if (sep >= 0) {
		page = p.slice(sep + 3);
		p = p.slice(0, sep);
	} else if (p.endsWith('/-')) {
		p = p.slice(0, -2);
	}
	p = p.replace(/\/+$/, '');
	const at = p.indexOf('@');
	const repo = decode(at >= 0 ? p.slice(0, at) : p);
	const rev = at >= 0 ? decode(p.slice(at + 1)) : '';
	if (repo === '' || repo.startsWith('-')) return null;

	let kind: RepoPageKind = 'root';
	let path = '';
	if (page !== '') {
		const slash = page.indexOf('/');
		const k = slash >= 0 ? page.slice(0, slash) : page;
		if (!PAGE_KINDS.has(k)) return null;
		kind = k as RepoPageKind;
		path = decode(slash >= 0 ? page.slice(slash + 1) : '').replace(/^\/+|\/+$/g, '');
	}
	return { repo, rev: rev === '' ? null : rev, kind, path };
}

/** Encodes a repo name, revision or path for a URL path, keeping `/` and other readable characters. */
export function encodePath(s: string): string {
	return s
		.split('/')
		.map((seg) =>
			encodeURIComponent(seg).replace(/%40/g, '@').replace(/%2B/g, '+').replace(/%3A/g, ':')
		)
		.join('/');
}

/** `/{repo}[@{rev}]`. */
export function repoURL(repo: string, rev?: string | null): string {
	return rev ? `/${encodePath(repo)}@${encodePath(rev)}` : `/${encodePath(repo)}`;
}

/** `/{repo}[@{rev}]/-/{kind}[/{path}]`; `root` gives the repository URL. */
export function repoPageURL(
	repo: string,
	rev: string | null | undefined,
	kind: RepoPageKind | 'raw',
	path = ''
): string {
	const base = repoURL(repo, rev);
	if (kind === 'root') return base;
	return path ? `${base}/-/${kind}/${encodePath(path)}` : `${base}/-/${kind}`;
}

/** URL of the same page at another revision (for the revision switcher). */
export function withRevision(route: RepoRoute, rev: string | null): string {
	switch (route.kind) {
		case 'commit':
		case 'compare':
		case 'branches':
		case 'tags':
		case 'code-graph':
		case 'settings':
			return repoURL(route.repo, rev);
		default:
			return repoPageURL(route.repo, rev, route.kind, route.path);
	}
}

/** The parent directory of a path (`''` for top-level entries). */
export function parentPath(path: string): string {
	const i = path.lastIndexOf('/');
	return i < 0 ? '' : path.slice(0, i);
}

/** Every ancestor directory of a path, outermost first, excluding the root. */
export function ancestorPaths(path: string): string[] {
	const parts = path.split('/').filter(Boolean);
	const out: string[] = [];
	for (let i = 1; i < parts.length; i++) out.push(parts.slice(0, i).join('/'));
	return out;
}

/** A 1-based inclusive line range. */
export interface LineRange {
	start: number;
	end: number;
}

/** Parses `#L10`, `#L10-20` or `#L10:3-20:5` (columns ignored); the range is normalized. */
export function parseLineRange(hash: string): LineRange | null {
	const m = /^#?L(\d+)(?::\d+)?(?:-L?(\d+)(?::\d+)?)?$/.exec(hash.trim());
	if (!m) return null;
	const a = Number(m[1]);
	const b = m[2] === undefined ? a : Number(m[2]);
	if (a < 1 || b < 1) return null;
	return { start: Math.min(a, b), end: Math.max(a, b) };
}

/** `L10` or `L10-20` (without the `#`). */
export function formatLineRange(r: LineRange): string {
	return r.start === r.end ? `L${r.start}` : `L${r.start}-${r.end}`;
}

/** Extends a selection with a shift-click, or starts a new one. */
export function selectLine(current: LineRange | null, line: number, extend: boolean): LineRange {
	if (!extend || current === null) return { start: line, end: line };
	const anchor = line < current.start ? current.end : current.start;
	return { start: Math.min(anchor, line), end: Math.max(anchor, line) };
}

/** Splits `base...head` (or `base..head`) of a compare page. */
export function parseCompareSpec(spec: string): { base: string; head: string } {
	const three = spec.indexOf('...');
	if (three >= 0) return { base: spec.slice(0, three), head: spec.slice(three + 3) };
	const two = spec.indexOf('..');
	if (two >= 0) return { base: spec.slice(0, two), head: spec.slice(two + 2) };
	return { base: '', head: spec };
}

/** The first 7 characters of an oid; other revisions unchanged. */
export function shortRev(rev: string): string {
	return /^[0-9a-f]{40}$/.test(rev) ? rev.slice(0, 7) : rev;
}
