/**
 * Bulk changes credentials: the code hosts changesets are published to,
 * with the site's credential (site admin) or the user's (else the site's), add and remove.
 */
import type { SiteBatchChangesCodeHostsQuery, UserBatchChangesCodeHostsQuery } from '#lib/graphql-types.ts';

const CODE_HOST_FIELDS = `
	totalCount
	nodes {
		externalServiceKind externalServiceURL requiresSSH requiresUsername hasWebhooks
		credential { id isSiteCredential createdAt sshPublicKey }
	}`;

export const SITE_CODE_HOSTS = `query SiteBatchChangesCodeHosts($first: Int) {
	batchChangesCodeHosts(first: $first) { ${CODE_HOST_FIELDS} }
}`;

export const USER_CODE_HOSTS = `query UserBatchChangesCodeHosts($first: Int) {
	currentUser { id batchChangesCodeHosts(first: $first) { ${CODE_HOST_FIELDS} } }
}`;

export type SiteCodeHostsResult = SiteBatchChangesCodeHostsQuery;
export type CodeHostConnection = SiteCodeHostsResult['batchChangesCodeHosts'];
export type BatchChangesCodeHost = CodeHostConnection['nodes'][number];

export type UserCodeHostsResult = UserBatchChangesCodeHostsQuery;

export const CREATE_CREDENTIAL = `mutation CreateBatchChangesCredential($user: ID, $externalServiceKind: ExternalServiceKind!, $externalServiceURL: String!, $username: String, $credential: String!) {
	createBatchChangesCredential(user: $user, externalServiceKind: $externalServiceKind, externalServiceURL: $externalServiceURL, username: $username, credential: $credential) { id }
}`;

export const DELETE_CREDENTIAL = `mutation DeleteBatchChangesCredential($id: ID!) {
	deleteBatchChangesCredential(batchChangesCredential: $id) { alwaysNil }
}`;

/** How a code host's credential reads in the list. */
export function credentialStatus(
	host: BatchChangesCodeHost,
	scope: 'site' | 'user'
): 'none' | 'own' | 'site' {
	const c = host.credential;
	if (!c) return 'none';
	if (scope === 'user' && c.isSiteCredential) return 'site';
	return 'own';
}

/** The code host's host name for display (`gitlab.example.com`), or the URL as is. */
export function codeHostName(url: string): string {
	try {
		return new URL(url).host || url;
	} catch {
		return url;
	}
}

/** The code host's page for creating a personal access token with the `api` scope. */
export function tokenPageURL(url: string): string {
	return `${url.replace(/\/+$/, '')}/-/user_settings/personal_access_tokens?name=Sourcegull+batch+changes&scopes=api,read_user,read_repository,write_repository`;
}
