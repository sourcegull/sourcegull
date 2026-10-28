/**
 * Helpers of the guided `auth.providers` editor: provider types, the OAuth redirect URIs the
 * server uses (`crates/http-api`: `/.auth/<type>/callback`) and where the code hosts list OAuth
 * applications.
 */
import type { JsonSchema } from './site-config-form.ts';

export type ProviderType = 'builtin' | 'gitlab' | 'github' | 'gitea' | 'openidconnect' | 'http-header';

export interface ProviderTypeInfo {
	type: ProviderType;
	label: string;
	/** One line shown in the "Add provider" menu. */
	blurb: string;
	/** Whether new providers of this type can be added. */
	addable: boolean;
}

export const PROVIDER_TYPES: ProviderTypeInfo[] = [
	{ type: 'gitlab', label: 'GitLab', blurb: 'Sign in with gitlab.com or a self-managed GitLab (OAuth).', addable: true },
	{ type: 'github', label: 'GitHub', blurb: 'Sign in with github.com or GitHub Enterprise Server (OAuth app).', addable: true },
	{ type: 'gitea', label: 'Gitea', blurb: 'Sign in with a Gitea instance (OAuth2 application).', addable: true },
	{
		type: 'openidconnect',
		label: 'OpenID Connect',
		blurb: 'Okta, Keycloak, Microsoft Entra ID, Google and other OIDC identity providers.',
		addable: true
	},
	{ type: 'builtin', label: 'Username and password', blurb: 'Accounts stored in Sourcegull (built-in).', addable: true },
	{
		type: 'http-header',
		label: 'HTTP header',
		blurb: 'Trust a username header set by an authenticating reverse proxy.',
		addable: true
	}
];

/**
 * Whether the server supports providers of this type. Others (such as `saml`) are ignored when
 * a stored configuration is loaded and rejected when it is saved.
 */
export function isSupportedProviderType(type: unknown): boolean {
	return PROVIDER_TYPES.some((t) => t.type === type);
}

export function providerTypeInfo(type: unknown): ProviderTypeInfo {
	return (
		PROVIDER_TYPES.find((t) => t.type === type) ?? {
			type: 'builtin',
			label: type === 'saml' ? 'SAML' : String(type ?? 'Unknown'),
			blurb: '',
			addable: false
		}
	);
}

export const DEFAULT_GITLAB_URL = 'https://gitlab.com/';
export const DEFAULT_GITHUB_URL = 'https://github.com/';

/** OAuth callback paths, relative to `externalURL`. */
export const CALLBACK_PATHS: Partial<Record<ProviderType, string>> = {
	gitlab: '/.auth/gitlab/callback',
	github: '/.auth/github/callback',
	gitea: '/.auth/gitea/callback',
	openidconnect: '/.auth/openidconnect/callback'
};

/** Whether `url` is an absolute http(s) URL. */
export function isHttpURL(url: unknown): url is string {
	if (typeof url !== 'string' || url.trim() === '') return false;
	try {
		const u = new URL(url.trim());
		return u.protocol === 'http:' || u.protocol === 'https:';
	} catch {
		return false;
	}
}

/** The redirect URI to register at the identity provider, or null without a valid externalURL. */
export function redirectURI(externalURL: unknown, type: ProviderType): string | null {
	const path = CALLBACK_PATHS[type];
	if (!path || !isHttpURL(externalURL)) return null;
	return externalURL.trim().replace(/\/+$/, '') + path;
}

/** The GitLab base URL of a provider (`url`, default gitlab.com), without a trailing slash. */
export function gitlabBase(url: unknown): string {
	const u = typeof url === 'string' && url.trim() !== '' ? url.trim() : DEFAULT_GITLAB_URL;
	return u.replace(/\/+$/, '');
}

/** Where OAuth applications are created in GitLab: as a user, or instance-wide as an admin. */
export function gitlabApplicationURLs(url: unknown): { user: string; admin: string; group: string } {
	const base = gitlabBase(url);
	return {
		user: `${base}/-/user_settings/applications`,
		admin: `${base}/admin/applications`,
		group: `${base}/groups/<group>/-/settings/applications`
	};
}

/** The GitHub base URL of a provider (`url`, default github.com), without a trailing slash. */
export function githubBase(url: unknown): string {
	const u = typeof url === 'string' && url.trim() !== '' ? url.trim() : DEFAULT_GITHUB_URL;
	return u.replace(/\/+$/, '');
}

/** Where OAuth apps are created in GitHub: for the signed-in user, or for an organization. */
export function githubOAuthAppURLs(url: unknown): { user: string; org: string } {
	const base = githubBase(url);
	return { user: `${base}/settings/applications/new`, org: `${base}/organizations/<org>/settings/applications/new` };
}

/** The Gitea base URL of a provider (`url`, no default), without a trailing slash; empty when not set. */
export function giteaBase(url: unknown): string {
	return typeof url === 'string' ? url.trim().replace(/\/+$/, '') : '';
}

/** Where OAuth2 applications are created in Gitea: for the signed-in user, or instance-wide as an admin. */
export function giteaApplicationURLs(url: unknown): { user: string; admin: string } | null {
	const base = giteaBase(url);
	if (!isHttpURL(base)) return null;
	return { user: `${base}/user/settings/applications`, admin: `${base}/-/admin/applications` };
}

/** The scopes the GitLab OAuth application needs, and what each is for. */
export const GITLAB_SCOPES: { scope: string; purpose: string }[] = [
	{ scope: 'read_user', purpose: 'reads the signed-in user’s username, name and verified email to create or match the account.' },
	{
		scope: 'read_api',
		purpose:
			'lists the user’s groups (for Allow groups) and, when the code host connection uses the oauth identity provider, which projects they can access, so GitLab repository permissions apply in Sourcegull.'
	}
];

/** A new provider of `type` with its required fields present (empty, for the admin to fill). */
export function newProvider(type: ProviderType): Record<string, unknown> {
	switch (type) {
		case 'gitlab':
			return { type, url: DEFAULT_GITLAB_URL, clientID: '', clientSecret: '', allowSignup: true };
		case 'github':
			return { type, url: DEFAULT_GITHUB_URL, clientID: '', clientSecret: '', allowSignup: true };
		case 'gitea':
			return { type, url: '', clientID: '', clientSecret: '', allowSignup: true };
		case 'openidconnect':
			return { type, issuer: '', clientID: '', clientSecret: '', allowSignup: true };
		case 'http-header':
			return { type, usernameHeader: 'X-Forwarded-User' };
		default:
			return { type: 'builtin', allowSignup: false };
	}
}

/** The schema variant of `auth.providers` items for `type` (from the `AuthProvider` oneOf). */
export function providerVariant(schema: JsonSchema, type: unknown): JsonSchema | null {
	const variants = schema.definitions?.AuthProvider?.oneOf ?? [];
	return variants.find((v) => v.properties?.type?.enum?.[0] === type) ?? null;
}

/** Required fields of a provider that are missing or empty. */
export function missingRequired(variant: JsonSchema | null, provider: Record<string, unknown>): string[] {
	return (variant?.required ?? []).filter((k) => k !== 'type').filter((k) => {
		const v = provider[k];
		return v === undefined || v === null || (typeof v === 'string' && v.trim() === '');
	});
}

/** One line describing a provider in the list. */
export function providerSummary(p: Record<string, unknown>): string {
	switch (p.type) {
		case 'gitlab':
			return `${gitlabBase(p.url)}${p.allowSignup ? ' · new users can sign up' : ' · existing users only'}`;
		case 'github':
			return `${githubBase(p.url)}${p.allowSignup ? ' · new users can sign up' : ' · existing users only'}`;
		case 'gitea':
			return `${giteaBase(p.url) || 'URL not set'}${p.allowSignup ? ' · new users can sign up' : ' · existing users only'}`;
		case 'openidconnect':
			return `${typeof p.issuer === 'string' && p.issuer ? p.issuer : 'issuer not set'}${p.allowSignup ? ' · new users can sign up' : ''}`;
		case 'builtin':
			return p.allowSignup ? 'Anyone can create an account' : 'Only admins create accounts';
		case 'http-header':
			return `Username from ${String(p.usernameHeader ?? '?')}`;
		default:
			return '';
	}
}
