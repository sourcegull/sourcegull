/**
 * The form view of a GitLab connection's configuration: which projects are mirrored
 * (`projectQuery`, `projects`), exclusions (`exclude`) and the token page link. Each reader
 * takes the parsed document; each writer returns the `[path, value]` edits to apply with
 * `setAt`, so settings the form doesn't show are left as they are.
 */
import type { ConfigPath } from '#lib/site-config-form.ts';

/** A change to apply to the document; `undefined` removes the key. */
export type Edit = [ConfigPath, unknown];

/** Every project the token's user is a member of, archived ones skipped. */
export const MEMBERSHIP_QUERY = 'projects?membership=true&archived=no';

/** How the projects to mirror are chosen. */
export type ProjectMode = 'membership' | 'selected' | 'custom';

export interface ProjectSelection {
	mode: ProjectMode;
	/** Group full paths (`selected`). */
	groups: string[];
	/** Project full paths or numeric ids (`projects`). */
	projects: string[];
	/** Raw `projectQuery` entries (`custom`). */
	queries: string[];
}

const GROUP_QUERY = /^groups\/([^/?]+)\/projects\?include_subgroups=true&archived=no$/;

/** The `projectQuery` entry for every project of `group` and its subgroups. */
export function groupQuery(group: string): string {
	return `groups/${encodeURIComponent(group)}/projects?include_subgroups=true&archived=no`;
}

/**
 * A group or project path as typed or pasted: a full URL on `base` (or any host), with or
 * without a trailing slash or a `/-/…` page suffix, becomes `group/sub`.
 */
export function normalizePath(input: string): string {
	let s = input.trim();
	const m = /^https?:\/\/[^/]+\/(.*)$/.exec(s);
	if (m) s = m[1] ?? '';
	s = s.replace(/\/-\/.*$/, '').replace(/^\/+|\/+$/g, '');
	s = s.replace(/\.git$/, '');
	return s;
}

/** Non-empty lines of a textarea, normalized and without duplicates. */
export function lines(text: string, normalize: (s: string) => string = (s) => s.trim()): string[] {
	const out: string[] = [];
	for (const raw of text.split('\n')) {
		const v = normalize(raw);
		if (v && !out.includes(v)) out.push(v);
	}
	return out;
}

function strings(v: unknown): string[] {
	return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
}

function projectRefs(v: unknown): string[] {
	if (!Array.isArray(v)) return [];
	const out: string[] = [];
	for (const p of v) {
		if (!p || typeof p !== 'object') continue;
		const r = p as Record<string, unknown>;
		if (typeof r.name === 'string') out.push(r.name);
		else if (typeof r.id === 'number') out.push(String(r.id));
	}
	return out;
}

/** The project selection of a parsed configuration. */
export function readProjects(doc: Record<string, unknown>): ProjectSelection {
	const queries = strings(doc.projectQuery ?? ['none']).filter((q) => q !== 'none');
	const projects = projectRefs(doc.projects);
	if (queries.length === 1 && queries[0] === MEMBERSHIP_QUERY && projects.length === 0) {
		return { mode: 'membership', groups: [], projects, queries };
	}
	const groups: string[] = [];
	for (const q of queries) {
		const m = GROUP_QUERY.exec(q);
		if (!m) return { mode: 'custom', groups: [], projects, queries };
		groups.push(decodeURIComponent(m[1] ?? ''));
	}
	return { mode: 'selected', groups, projects, queries };
}

/** The edits that store `sel`. */
export function projectEdits(sel: ProjectSelection): Edit[] {
	const projects = sel.projects.map((p) => (/^\d+$/.test(p) ? { id: Number(p) } : { name: p }));
	switch (sel.mode) {
		case 'membership':
			return [
				[['projectQuery'], [MEMBERSHIP_QUERY]],
				[['projects'], undefined]
			];
		case 'selected':
			return [
				[['projectQuery'], sel.groups.length > 0 ? sel.groups.map(groupQuery) : undefined],
				[['projects'], projects.length > 0 ? projects : undefined]
			];
		case 'custom':
			return [
				[['projectQuery'], sel.queries.length > 0 ? sel.queries : undefined],
				[['projects'], projects.length > 0 ? projects : undefined]
			];
	}
}

/** The `exclude` entries the form shows, and the ones it keeps as they are. */
export interface Exclusions {
	names: string[];
	patterns: string[];
	emptyRepos: boolean;
	/** Entries by id or with several fields, kept unchanged. */
	other: unknown[];
}

export function readExclusions(doc: Record<string, unknown>): Exclusions {
	const ex: Exclusions = { names: [], patterns: [], emptyRepos: false, other: [] };
	for (const e of Array.isArray(doc.exclude) ? doc.exclude : []) {
		const keys = e && typeof e === 'object' ? Object.keys(e) : [];
		const r = e as Record<string, unknown>;
		if (keys.length === 1 && typeof r.name === 'string') ex.names.push(r.name);
		else if (keys.length === 1 && typeof r.pattern === 'string') ex.patterns.push(r.pattern);
		else if (keys.length === 1 && r.emptyRepos === true) ex.emptyRepos = true;
		else ex.other.push(e);
	}
	return ex;
}

/** The `exclude` value of `ex` (undefined when empty). */
export function exclusionsValue(ex: Exclusions): unknown[] | undefined {
	const list = [
		...ex.other,
		...ex.names.map((name) => ({ name })),
		...ex.patterns.map((pattern) => ({ pattern })),
		...(ex.emptyRepos ? [{ emptyRepos: true }] : [])
	];
	return list.length > 0 ? list : undefined;
}

/** `url` without trailing slashes, or gitlab.com when it isn't an http(s) URL. */
export function gitlabBase(url: unknown): string {
	return typeof url === 'string' && /^https?:\/\/[^/]/.test(url.trim())
		? url.trim().replace(/\/+$/, '')
		: 'https://gitlab.com';
}

/** GitLab's "new personal access token" page with the name and `scopes` filled in. */
export function tokenPageURL(url: unknown, scopes: string[]): string {
	return `${gitlabBase(url)}/-/user_settings/personal_access_tokens?name=Sourcegull&scopes=${scopes.join(',')}`;
}

/** The top-level keys the form edits; anything else is only in the JSON view. */
export const FORM_KEYS = [
	'url',
	'token',
	'projectQuery',
	'projects',
	'exclude',
	'authorization',
	'gitURLType',
	'markInternalReposAsPublic',
	'webhooks.autoRegister',
	'rateLimit',
	'certificate',
	'repositoryPathPattern'
];

/** Keys managed elsewhere (the webhook secret, on the connection's page). */
const MANAGED_KEYS = ['webhooks'];

/** Keys set in the document that the form doesn't show. */
export function jsonOnlyKeys(doc: Record<string, unknown>): string[] {
	return Object.keys(doc).filter((k) => !FORM_KEYS.includes(k) && !MANAGED_KEYS.includes(k));
}
