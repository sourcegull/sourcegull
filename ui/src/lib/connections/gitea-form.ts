/**
 * The form view of a Gitea connection's configuration. The repository selection
 * (`repositoryQuery`, `orgs`, `repos`) works as for GitHub and reuses its readers and writers;
 * this module adds what differs: pasted URLs of an instance served under a path, the `mirrors`
 * exclusion and the token page.
 */
import {
	exclusionsValue as githubExclusionsValue,
	readExclusions as githubReadExclusions,
	normalizeRepo as githubNormalizeRepo,
	type Exclusions as GitHubExclusions
} from './github-form.ts';

export { AFFILIATED, jsonOnlyKeys, readRepos, repoEdits, type RepoMode } from './github-form.ts';

/** The `repositoryQuery` entry for every repository the token can read. */
export const ALL = 'all';

/** `url` without trailing slashes, or an example URL when it isn't an http(s) URL. */
export function giteaBase(url: unknown): string {
	return typeof url === 'string' && /^https?:\/\/[^/]/.test(url.trim())
		? url.trim().replace(/\/+$/, '')
		: 'https://gitea.example.com';
}

/** `input` with the instance's base URL (which may have a path) removed from its start. */
function withoutBase(input: string, base: string): string {
	const s = input.trim();
	return s.toLowerCase().startsWith(`${base.toLowerCase()}/`) ? s.slice(base.length + 1) : s;
}

/** A repository as typed or pasted (`owner/name`, or its URL, even of a file in it) → `owner/name`. */
export function normalizeRepo(input: string, url?: unknown): string {
	return githubNormalizeRepo(withoutBase(input, giteaBase(url)));
}

/** An organization as typed or pasted (its name, or the URL of its page or settings) → the name. */
export function normalizeOrg(input: string, url?: unknown): string {
	return normalizeRepo(input, url).replace(/^org\//, '').split('/')[0] ?? '';
}

/** The `exclude` entries the form shows, and the ones it keeps as they are. */
export interface Exclusions extends GitHubExclusions {
	mirrors: boolean;
}

function isMirrorsOnly(e: unknown): boolean {
	return !!e && typeof e === 'object' && Object.keys(e).length === 1 && (e as Record<string, unknown>).mirrors === true;
}

export function readExclusions(doc: Record<string, unknown>): Exclusions {
	const ex = githubReadExclusions(doc);
	const other = ex.other.filter((e) => !isMirrorsOnly(e));
	return { ...ex, mirrors: other.length !== ex.other.length, other };
}

/** The `exclude` value of `ex` (undefined when empty). */
export function exclusionsValue(ex: Exclusions): unknown[] | undefined {
	const list = [...(githubExclusionsValue(ex) ?? []), ...(ex.mirrors ? [{ mirrors: true }] : [])];
	return list.length > 0 ? list : undefined;
}

/** Gitea's "Applications" settings page, where access tokens are created. */
export function tokenURL(url: unknown): string {
	return `${giteaBase(url)}/user/settings/applications`;
}

/** The `authorization.identityProvider.type` the form shows: `oauth`, `username` or `none`. */
export function identityProvider(doc: Record<string, unknown>): 'oauth' | 'username' | 'none' {
	const a = doc.authorization as { identityProvider?: { type?: unknown } } | undefined;
	const t = a?.identityProvider?.type;
	return t === 'oauth' || t === 'username' ? t : 'none';
}
