/**
 * The steps that make a new instance useful, derived from the site configuration and a few
 * counts: the overview page's setup checklist and the "next steps" of a code host connection.
 */
import { githubBase, gitlabBase, giteaBase } from '#lib/auth-providers.ts';
import { kindLabel, type ExternalServiceKind } from '#lib/connections/queries.ts';
import { parseJsoncObject } from '#lib/site-config-sections.ts';
import { configSectionURL } from './related.ts';

export interface SetupStep {
	id: string;
	title: string;
	text: string;
	done: boolean;
	href: string;
	linkText: string;
}

export interface SetupState {
	/** The site configuration document (JSONC), or null when it could not be read. */
	siteConfig: string | null;
	connections: number;
	users: number;
	emailEnabled: boolean;
}

type Doc = Record<string, unknown>;

function providers(doc: Doc | null): Doc[] {
	const list = doc?.['auth.providers'];
	return Array.isArray(list) ? list.filter((p): p is Doc => typeof p === 'object' && p !== null) : [];
}

/** `url` without a trailing slash and in lower case, for comparing code host URLs. */
function normalize(url: string): string {
	return url.trim().replace(/\/+$/, '').toLowerCase();
}

/** Whether a sign-in provider for the code host at `url` is configured. */
export function hasSignInProvider(siteConfig: string | null, kind: ExternalServiceKind, url: string): boolean {
	const target = normalize(url);
	return providers(parseJsoncObject(siteConfig ?? '')).some((p) => {
		switch (kind) {
			case 'GITLAB':
				return p.type === 'gitlab' && normalize(gitlabBase(p.url)) === target;
			case 'GITHUB':
				return p.type === 'github' && normalize(githubBase(p.url)) === target;
			case 'GITEA':
				return p.type === 'gitea' && normalize(giteaBase(p.url)) === target;
		}
	});
}

/** Whether a connection's configuration (JSONC) has an `authorization` block. */
export function enforcesPermissions(connectionConfig: string): boolean {
	const auth = parseJsoncObject(connectionConfig)?.authorization;
	return typeof auth === 'object' && auth !== null;
}

/** The overview page's checklist, in the order an admin works through it. */
export function setupSteps(s: SetupState): SetupStep[] {
	const doc = s.siteConfig === null ? null : parseJsoncObject(s.siteConfig);
	const externalURL = doc?.externalURL;
	const signIn = providers(doc).some((p) => p.type !== 'builtin');
	return [
		{
			id: 'external-url',
			title: 'Set the external URL',
			text: 'The URL people reach this instance at. Links in email, OAuth redirects and agent setup use it.',
			done: typeof externalURL === 'string' && externalURL.trim() !== '',
			href: configSectionURL('general'),
			linkText: 'Set it'
		},
		{
			id: 'connection',
			title: 'Connect a code host',
			text: 'Add a GitLab, GitHub or Gitea connection; its repositories are cloned and indexed for search.',
			done: s.connections > 0,
			href: '/site-admin/external-services/new',
			linkText: 'Add connection'
		},
		{
			id: 'sign-in',
			title: 'Let people sign in with your code host or identity provider',
			text: 'Add a GitLab, GitHub, Gitea or OpenID Connect sign-in provider, so nobody needs another password.',
			done: signIn,
			href: configSectionURL('auth'),
			linkText: 'Add a provider'
		},
		{
			id: 'email',
			title: 'Set up email',
			text: 'Password resets, invitations and notifications are sent by email.',
			done: s.emailEnabled,
			href: configSectionURL('notifications'),
			linkText: 'Set up email'
		},
		{
			id: 'users',
			title: 'Invite people',
			text: 'Share the sign-in page, or create accounts and send setup links.',
			done: s.users > 1,
			href: '/site-admin/users',
			linkText: 'Users'
		}
	];
}

/** What is still missing for a connection's users: sign-in through its host and permissions. */
export function connectionNextSteps(
	siteConfig: string | null,
	c: { kind: ExternalServiceKind; url: string; config: string }
): SetupStep[] {
	const host = kindLabel(c.kind);
	const signIn = hasSignInProvider(siteConfig, c.kind, c.url);
	return [
		{
			id: 'sign-in',
			title: `Let people sign in with ${host}`,
			text: `Add a ${host} sign-in provider for ${c.url}, so people use their ${host} accounts here.`,
			done: signIn,
			href: configSectionURL('auth'),
			linkText: 'Add a provider'
		},
		{
			id: 'permissions',
			title: `Use ${host} repository permissions`,
			text: signIn
				? `Add an "authorization" block to this connection, so people see only the repositories they can read on ${host}.`
				: `Once ${host} sign-in is set up, add an "authorization" block to this connection, so people see only the repositories they can read on ${host}.`,
			done: enforcesPermissions(c.config),
			href: '',
			linkText: 'Edit connection'
		}
	];
}
