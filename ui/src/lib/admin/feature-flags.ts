/**
 * GraphQL operations and helpers of the site admin Feature flags page: boolean and rollout flags
 * with per-user and per-org overrides. Flags the resources file owns are read-only; their overrides
 * stay editable.
 */
import { DECLARED_KINDS_FIELDS, type DeclaredKindsFile } from '#lib/declared-config.ts';
import type { FeatureFlagNamespaceQuery, FeatureFlagsQuery } from '#lib/graphql-types.ts';

const OVERRIDE_FIELDS = `id value namespace { __typename id ... on User { username } ... on Org { name } }`;

export const FEATURE_FLAGS = `query FeatureFlags {
	featureFlags {
		__typename
		... on FeatureFlagBoolean { name value createdAt updatedAt managedBy overrides { ${OVERRIDE_FIELDS} } }
		... on FeatureFlagRollout { name rolloutBasisPoints createdAt updatedAt managedBy overrides { ${OVERRIDE_FIELDS} } }
	}
	${DECLARED_KINDS_FIELDS}
}`;

export const CREATE_FEATURE_FLAG = `mutation CreateFeatureFlag($name: String!, $value: Boolean, $rolloutBasisPoints: Int) {
	createFeatureFlag(name: $name, value: $value, rolloutBasisPoints: $rolloutBasisPoints) { __typename }
}`;

export const UPDATE_FEATURE_FLAG = `mutation UpdateFeatureFlag($name: String!, $value: Boolean, $rolloutBasisPoints: Int) {
	updateFeatureFlag(name: $name, value: $value, rolloutBasisPoints: $rolloutBasisPoints) { __typename }
}`;

export const DELETE_FEATURE_FLAG = `mutation DeleteFeatureFlag($name: String!) {
	deleteFeatureFlag(name: $name) { alwaysNil }
}`;

export const CREATE_OVERRIDE = `mutation CreateFeatureFlagOverride($namespace: ID!, $flagName: String!, $value: Boolean!) {
	createFeatureFlagOverride(namespace: $namespace, flagName: $flagName, value: $value) { id }
}`;

export const UPDATE_OVERRIDE = `mutation UpdateFeatureFlagOverride($id: ID!, $value: Boolean!) {
	updateFeatureFlagOverride(id: $id, value: $value) { id }
}`;

export const DELETE_OVERRIDE = `mutation DeleteFeatureFlagOverride($id: ID!) {
	deleteFeatureFlagOverride(id: $id) { alwaysNil }
}`;

/** Resolves a user name and an org name to namespace IDs (either may be missing). */
export const NAMESPACE_IDS = `query FeatureFlagNamespace($name: String!) {
	user(username: $name) { id }
	organization(name: $name) { id }
}`;

export interface FlagOverride {
	id: string;
	value: boolean;
	namespace:
		| { __typename: 'User'; id: string; username: string }
		| { __typename: 'Org'; id: string; name: string };
}

interface FlagBase {
	name: string;
	createdAt: string;
	updatedAt: string;
	/** The resources file entry that owns the flag. */
	managedBy: string | null;
	overrides: FlagOverride[];
}

export type FeatureFlag =
	| (FlagBase & { __typename: 'FeatureFlagBoolean'; value: boolean })
	| (FlagBase & {
			__typename: 'FeatureFlagRollout';
			rolloutBasisPoints: number;
	  });

export type FeatureFlagsResult = FeatureFlagsQuery;

export type NamespaceIdsResult = FeatureFlagNamespaceQuery;

/** The variables for a flag's value: a boolean, or a rollout percentage (0–100, two decimals). */
export function flagValueVariables(
	kind: 'bool' | 'rollout',
	value: boolean,
	percent: number
): { value: boolean | null; rolloutBasisPoints: number | null } {
	if (kind === 'bool') return { value, rolloutBasisPoints: null };
	const bp = Math.round(Math.min(100, Math.max(0, percent)) * 100);
	return { value: null, rolloutBasisPoints: bp };
}

/** The flag's value in words: `on`, `off` or `25% of users`. */
export function describeFlag(f: FeatureFlag): string {
	if (f.__typename === 'FeatureFlagBoolean') return f.value ? 'on' : 'off';
	return `${f.rolloutBasisPoints / 100}% of users`;
}

/** The name of an override's user or org. */
export function namespaceName(o: FlagOverride): string {
	return o.namespace.__typename === 'User' ? o.namespace.username : o.namespace.name;
}
