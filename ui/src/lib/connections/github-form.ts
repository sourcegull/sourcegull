/**
 * The form view of a GitHub connection's configuration: how it authenticates (`token` or
 * `githubApp`), which repositories are mirrored (`repositoryQuery`, `orgs`, `repos`), exclusions
 * (`exclude`) and the token page links. Each
 * reader takes the parsed document; each writer returns the `[path, value]` edits to apply with
 * `setAt`, so settings the form doesn't show are left as they are.
 */
import type { Edit } from './gitlab-form.ts';

/** The `repositoryQuery` entry for the token user's repositories. */
export const AFFILIATED = 'affiliated';

/** How the repositories to mirror are chosen. */
export type RepoMode = 'affiliated' | 'selected' | 'custom';

export interface RepoSelection {
	mode: RepoMode;
	/** Organization logins (`orgs`). */
	orgs: string[];
	/** `owner/name` (`repos`). */
	repos: string[];
	/** Raw `repositoryQuery` entries (`custom`). */
	queries: string[];
}

/** The path of a GitHub URL (any host), without slashes at either end or a `.git` suffix. */
function urlPath(input: string): string {
	let s = input.trim();
	const m = /^https?:\/\/[^/]+\/(.*)$/.exec(s);
	if (m) s = m[1] ?? '';
	return s
		.replace(/[?#].*$/, '')
		.replace(/^\/+|\/+$/g, '')
		.replace(/\.git$/, '');
}

/** A repository as typed or pasted (`owner/name`, or its URL, even of a file in it) → `owner/name`. */
export function normalizeRepo(input: string): string {
	return urlPath(input).split('/').slice(0, 2).join('/');
}

/** An organization as typed or pasted (its login, or its URL) → the login. */
export function normalizeOrg(input: string): string {
	const path = urlPath(input).replace(/^orgs\//, '');
	return path.split('/')[0] ?? '';
}

function strings(v: unknown): string[] {
	return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
}

/** The repository selection of a parsed configuration. */
export function readRepos(doc: Record<string, unknown>): RepoSelection {
	const queries = strings(doc.repositoryQuery ?? ['none']).filter((q) => q !== 'none');
	const orgs = strings(doc.orgs);
	const repos = strings(doc.repos);
	if (queries.length === 1 && queries[0] === AFFILIATED && orgs.length === 0 && repos.length === 0) {
		return { mode: 'affiliated', orgs, repos, queries };
	}
	return { mode: queries.length === 0 ? 'selected' : 'custom', orgs, repos, queries };
}

/** The edits that store `sel`. */
export function repoEdits(sel: RepoSelection): Edit[] {
	const list = (v: string[]) => (v.length > 0 ? v : undefined);
	switch (sel.mode) {
		case 'affiliated':
			return [
				[['repositoryQuery'], [AFFILIATED]],
				[['orgs'], undefined],
				[['repos'], undefined]
			];
		case 'selected':
			return [
				[['repositoryQuery'], undefined],
				[['orgs'], list(sel.orgs)],
				[['repos'], list(sel.repos)]
			];
		case 'custom':
			return [
				[['repositoryQuery'], list(sel.queries)],
				[['orgs'], list(sel.orgs)],
				[['repos'], list(sel.repos)]
			];
	}
}

/** The `exclude` entries the form shows, and the ones it keeps as they are. */
export interface Exclusions {
	names: string[];
	patterns: string[];
	archived: boolean;
	forks: boolean;
	/** Entries by id or with several fields, kept unchanged. */
	other: unknown[];
}

export function readExclusions(doc: Record<string, unknown>): Exclusions {
	const ex: Exclusions = { names: [], patterns: [], archived: false, forks: false, other: [] };
	for (const e of Array.isArray(doc.exclude) ? doc.exclude : []) {
		const keys = e && typeof e === 'object' ? Object.keys(e) : [];
		const r = e as Record<string, unknown>;
		if (keys.length === 1 && typeof r.name === 'string') ex.names.push(r.name);
		else if (keys.length === 1 && typeof r.pattern === 'string') ex.patterns.push(r.pattern);
		else if (keys.length === 1 && r.archived === true) ex.archived = true;
		else if (keys.length === 1 && r.forks === true) ex.forks = true;
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
		...(ex.archived ? [{ archived: true }] : []),
		...(ex.forks ? [{ forks: true }] : [])
	];
	return list.length > 0 ? list : undefined;
}

/** `url` without trailing slashes, or github.com when it isn't an http(s) URL. */
export function githubBase(url: unknown): string {
	return typeof url === 'string' && /^https?:\/\/[^/]/.test(url.trim())
		? url.trim().replace(/\/+$/, '')
		: 'https://github.com';
}

/** GitHub's "new fine-grained personal access token" page. */
export function fineGrainedTokenURL(url: unknown): string {
	return `${githubBase(url)}/settings/personal-access-tokens/new`;
}

/** GitHub's "new classic personal access token" page with the note and the `repo` scope filled in. */
export function classicTokenURL(url: unknown): string {
	return `${githubBase(url)}/settings/tokens/new?description=Sourcegull&scopes=repo`;
}

/** How the connection authenticates: a personal access token, or as a GitHub App installation. */
export type AuthMethod = 'token' | 'app';

/** The `githubApp` fields as the form shows them (IDs as typed). */
export interface AppFields {
	appID: string;
	privateKey: string;
	installationID: string;
}

export function readAuthMethod(doc: Record<string, unknown>): AuthMethod {
	return doc.githubApp && typeof doc.githubApp === 'object' && doc.token === undefined ? 'app' : 'token';
}

export function readApp(doc: Record<string, unknown>): AppFields {
	const app = (doc.githubApp && typeof doc.githubApp === 'object' ? doc.githubApp : {}) as Record<string, unknown>;
	const text = (v: unknown) => (typeof v === 'number' || typeof v === 'string' ? String(v) : '');
	return { appID: text(app.appID), privateKey: text(app.privateKey), installationID: text(app.installationID) };
}

/**
 * The edits that switch to `method`: `token` and `githubApp` are exclusive. `token` is the
 * value the token field starts with (the stored `REDACTED` when editing a token connection).
 */
export function authMethodEdits(method: AuthMethod, token: string): Edit[] {
	return method === 'app'
		? [
				[['token'], undefined],
				[['githubApp'], { appID: 0, privateKey: '', installationID: 0 }]
			]
		: [
				[['githubApp'], undefined],
				[['token'], token]
			];
}

/**
 * The edit that stores one `githubApp` field. IDs are numbers (0 while empty, which validation
 * rejects); an emptied private key keeps the stored one when `editing`.
 */
export function appFieldEdit(field: keyof AppFields, raw: string, editing: boolean): Edit {
	const v = raw.trim();
	if (field === 'privateKey') {
		return [['githubApp', 'privateKey'], v === '' && editing ? 'REDACTED' : v === '' ? '' : v + '\n'];
	}
	const n = Number(v);
	return [['githubApp', field], v !== '' && Number.isSafeInteger(n) && n > 0 ? n : 0];
}

/** GitHub's "register a new GitHub App" page (personal account). */
export function newAppURL(url: unknown): string {
	return `${githubBase(url)}/settings/apps/new`;
}

/** The top-level keys the form edits; anything else is only in the JSON view. */
export const FORM_KEYS = [
	'url',
	'token',
	'githubApp',
	'repositoryQuery',
	'orgs',
	'repos',
	'exclude',
	'gitURLType',
	'rateLimit',
	'certificate',
	'repositoryPathPattern',
	'authorization',
	'markInternalReposAsPublic'
];

/** Keys managed elsewhere (the webhook secret, on the connection's page). */
const MANAGED_KEYS = ['webhooks'];

/** Keys set in the document that the form doesn't show. */
export function jsonOnlyKeys(doc: Record<string, unknown>): string[] {
	return Object.keys(doc).filter((k) => !FORM_KEYS.includes(k) && !MANAGED_KEYS.includes(k));
}
