/**
 * Configuration as code: the resources file (SOURCEGULL_RESOURCES_FILE) and the global settings file
 * (SOURCEGULL_GLOBAL_SETTINGS_FILE) the worker applies. Objects a file owns carry `managedBy` and are
 * read-only through the API; with `exclusive`, the file owns every object of its kinds and none
 * can be created in the UI.
 */
import type { DeclaredKindsQuery } from '#lib/graphql-types.ts';

/** The sections of the resources file, as `managedBy` and `kinds` name them. */
export type DeclaredKind =
	| 'connections'
	| 'searchContexts'
	| 'roles'
	| 'siteAdmins'
	| 'outboundWebhooks'
	| 'executorSecrets'
	| 'codeIntelPolicies'
	| 'featureFlags';

/** What each section declares, as notices and the configuration page's list of files name it. */
export const KIND_LABELS: Record<DeclaredKind, string> = {
	connections: 'code host connections',
	searchContexts: 'instance search contexts',
	roles: 'custom roles',
	siteAdmins: 'site admins',
	outboundWebhooks: 'outbound webhooks',
	executorSecrets: 'global executor secrets',
	codeIntelPolicies: 'site-wide code graph policies',
	featureFlags: 'feature flags'
};

/** The section's label, or its name for sections this UI does not know. */
export function kindLabel(kind: string): string {
	return KIND_LABELS[kind as DeclaredKind] ?? kind;
}

export type DeclaredSource = 'resources' | 'global_settings';

/** The environment variable that names a source's file. */
export function sourceVariable(source: string): string {
	return source === 'global_settings' ? 'SOURCEGULL_GLOBAL_SETTINGS_FILE' : 'SOURCEGULL_RESOURCES_FILE';
}

/** The fields pages select to know which kinds are exclusive (site admins only). */
export const DECLARED_KINDS_FIELDS = `site { declaredConfigFiles { source kinds exclusive } }`;

export const DECLARED_KINDS = `query DeclaredKinds {
	${DECLARED_KINDS_FIELDS}
}`;

export interface DeclaredKindsFile {
	source: string;
	kinds: string[];
	exclusive: boolean;
}

export type DeclaredKindsResult = DeclaredKindsQuery;

/** A configuration file in use and how its last application went. */
export interface DeclaredConfigFile extends DeclaredKindsFile {
	path: string;
	appliedAt: string | null;
	error: string | null;
	errorAt: string | null;
	checkedAt: string;
}

export const DECLARED_CONFIG_FILE_FIELDS = `source path kinds exclusive appliedAt error errorAt checkedAt`;

/** Whether the resources file owns every object of `kind`, so none can be created here. */
export function isExclusive(files: DeclaredKindsFile[] | null | undefined, kind: DeclaredKind): boolean {
	return (files ?? []).some((f) => f.source === 'resources' && f.exclusive && f.kinds.includes(kind));
}

/** Whether a `managedBy` value names the whole kind (exclusive) rather than one entry. */
export function managesWholeKind(managedBy: string): boolean {
	return !managedBy.includes('[');
}

/** The badge's tooltip for a managed object. */
export function managedTitle(managedBy: string): string {
	return `Managed by SOURCEGULL_RESOURCES_FILE (${managedBy}); change it in the file.`;
}
