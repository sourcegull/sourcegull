/** Repository key-value metadata (`repo:has.meta()`) on the repository settings page. */

import type { RepositoryMetadataQuery } from '#lib/graphql-types.ts';

export const REPOSITORY_METADATA = `query RepositoryMetadata($name: String!) {
	repository(name: $name) { id keyValuePairs { key value } }
	currentUser { id permissions { nodes { namespace action } } }
}`;

export const ADD_REPO_KEY_VALUE_PAIR = `mutation AddRepoKeyValuePair($repo: ID!, $key: String!, $value: String) {
	addRepoKeyValuePair(repo: $repo, key: $key, value: $value) { alwaysNil }
}`;

export const UPDATE_REPO_KEY_VALUE_PAIR = `mutation UpdateRepoKeyValuePair($repo: ID!, $key: String!, $value: String) {
	updateRepoKeyValuePair(repo: $repo, key: $key, value: $value) { alwaysNil }
}`;

export const DELETE_REPO_KEY_VALUE_PAIR = `mutation DeleteRepoKeyValuePair($repo: ID!, $key: String!) {
	deleteRepoKeyValuePair(repo: $repo, key: $key) { alwaysNil }
}`;

export const VIEWER_PERMISSIONS = `query ViewerPermissions {
	currentUser { id permissions { nodes { namespace action } } }
}`;

export type RepositoryMetadataResult = RepositoryMetadataQuery;

type Viewer = RepositoryMetadataQuery['currentUser'];

/** Whether the viewer holds `REPO_METADATA#WRITE`. */
export function canWriteMetadata(viewer: Viewer): boolean {
	return !!viewer?.permissions.nodes.some((p) => p.namespace === 'REPO_METADATA' && p.action === 'WRITE');
}

/**
 * The `repo:` filter that finds repositories with this pair (a tag when `value` is null), or null
 * when the key or value cannot be written in one (whitespace, parentheses, `:` in the key).
 */
export function metadataFilter(key: string, value: string | null): string | null {
	if (/[\s():]/.test(key) || (value !== null && /[\s()]/.test(value))) return null;
	return value === null ? `repo:has.tag(${key})` : `repo:has.meta(${key}:${value})`;
}
