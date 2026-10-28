/**
 * OAuth sign-in of MCP clients: the consent page's API (`/.api/oauth/authorizations/…`) and
 * Connected apps (the grants users gave, over GraphQL).
 */

import { REQUESTED_WITH_HEADER, REQUESTED_WITH_VALUE } from './graphql.ts';
import type { AllOAuthGrantsQuery, CurrentUserOAuthGrantsQuery } from '#lib/graphql-types.ts';

/** An authorization request waiting for the user's answer. */
export interface AuthorizationRequest {
	clientID: string;
	clientName: string;
	clientURI: string | null;
	/** Whether the client is identified by a metadata document at its `clientID` URL. */
	metadataDocument: boolean;
	redirectURI: string;
	redirectHost: string;
	/** Whether a self-registered client sends the code to a host that is not loopback. */
	redirectWarning: boolean;
	scopes: string[];
	resource: string;
	username: string;
}

/** An error from the consent API, with the server's message. */
export class AuthorizationError extends Error {
	constructor(
		message: string,
		readonly status: number
	) {
		super(message);
	}
}

async function call<T>(path: string, init: RequestInit, doFetch: typeof fetch): Promise<T> {
	let res: Response;
	try {
		res = await doFetch(path, {
			credentials: 'same-origin',
			...init,
			headers: { Accept: 'application/json', ...init.headers }
		});
	} catch (err) {
		throw new AuthorizationError(`Network error: ${String(err)}`, 0);
	}
	let json: { error?: string } & Record<string, unknown> = {};
	try {
		json = (await res.json()) as typeof json;
	} catch {
		// Non-JSON body: fall through to status handling.
	}
	if (!res.ok || typeof json.error === 'string') {
		throw new AuthorizationError(json.error ?? `Request failed (${res.status})`, res.status);
	}
	return json as T;
}

const requestPath = (request: string) => `/.api/oauth/authorizations/${encodeURIComponent(request)}`;

/**
 * The `/.oauth/authorize` URL to go back to with `query`, an app's request that waited for the
 * user to sign in (its metadata document is fetched only then). `null` unless `query` names a
 * client.
 */
export function authorizeURL(query: string | null): string | null {
	if (!query) return null;
	const params = new URLSearchParams(query);
	if (!params.get('client_id')) return null;
	return `/.oauth/authorize?${params.toString()}`;
}

/** Loads the authorization request with handle `request`. */
export function loadAuthorization(request: string, doFetch: typeof fetch = fetch): Promise<AuthorizationRequest> {
	return call<AuthorizationRequest>(requestPath(request), { method: 'GET' }, doFetch);
}

/** Answers the request; returns the URL to send the browser to (back to the client). */
export async function answerAuthorization(
	request: string,
	approve: boolean,
	doFetch: typeof fetch = fetch
): Promise<string> {
	const out = await call<{ redirectURL: string }>(
		requestPath(request),
		{
			method: 'POST',
			headers: { 'Content-Type': 'application/json', [REQUESTED_WITH_HEADER]: REQUESTED_WITH_VALUE },
			body: JSON.stringify({ approve })
		},
		doFetch
	);
	return out.redirectURL;
}

/** The name to show for a client: its own, or the host of its metadata document or redirect URI. */
export function clientLabel(c: { clientName: string; clientID: string; redirectHost?: string; redirectHosts?: string[] }): string {
	if (c.clientName.trim()) return c.clientName.trim();
	try {
		const url = new URL(c.clientID);
		if (url.protocol === 'https:') return url.host;
	} catch {
		// A registered client's ID is not a URL.
	}
	const host = c.redirectHost ?? c.redirectHosts?.[0];
	return host ? `App on ${host}` : 'Unnamed app';
}

export interface OAuthGrantNode {
	id: string;
	clientID: string;
	clientName: string;
	clientURI: string | null;
	redirectHosts: string[];
	scopes: string[];
	createdAt: string;
	lastUsedAt: string | null;
	user: { id: string; username: string };
}

const GRANT_FIELDS = `id
			clientID
			clientName
			clientURI
			redirectHosts
			scopes
			createdAt
			lastUsedAt
			user {
				id
				username
			}`;

export const CURRENT_USER_OAUTH_GRANTS = `query CurrentUserOAuthGrants {
	currentUser {
		id
		oauthGrants {
			${GRANT_FIELDS}
		}
	}
}`;

export type CurrentUserOAuthGrantsResult = CurrentUserOAuthGrantsQuery;

export const ALL_OAUTH_GRANTS = `query AllOAuthGrants {
	oauthGrants {
		${GRANT_FIELDS}
	}
}`;

export type AllOAuthGrantsResult = AllOAuthGrantsQuery;

export const REVOKE_OAUTH_GRANT = `mutation RevokeOAuthGrant($id: ID!) {
	revokeOAuthGrant(id: $id) {
		alwaysNil
	}
}`;

export interface RevokeOAuthGrantVariables {
	id: string;
}
