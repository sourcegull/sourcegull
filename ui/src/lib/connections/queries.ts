/**
 * GraphQL operations of the code host connections admin pages: list,
 * create, edit, delete, test connection, sync now, sync history and the webhook secret.
 */

import { DECLARED_KINDS_FIELDS, type DeclaredKindsFile } from '#lib/declared-config.ts';
import type {
	AddExternalServiceMutation,
	CheckExternalServiceConfigQuery,
	CheckExternalServiceConnectionQuery,
	ExternalServiceQuery,
	ExternalServicesQuery,
	RegenerateExternalServiceWebhookSecretMutation,
	UpdateExternalServiceMutation,
	ValidateExternalServiceConfigQuery
} from '#lib/graphql-types.ts';

export type ExternalServiceKind = 'GITLAB' | 'GITHUB' | 'GITEA';

/** Every connection kind, in the order the "Add connection" page offers them. */
export const KINDS: ExternalServiceKind[] = ['GITLAB', 'GITHUB', 'GITEA'];

/** The code host's name, e.g. "GitLab". */
export function kindLabel(kind: ExternalServiceKind): string {
	switch (kind) {
		case 'GITHUB':
			return 'GitHub';
		case 'GITEA':
			return 'Gitea';
		default:
			return 'GitLab';
	}
}

/** The kind named by a `?kind=` parameter (case-insensitive), or GitLab. */
export function parseKind(value: string | null): ExternalServiceKind {
	const upper = value?.toUpperCase();
	return KINDS.find((k) => k === upper) ?? 'GITLAB';
}

export type SyncJobState =
	'QUEUED' | 'PROCESSING' | 'ERRORED' | 'FAILED' | 'COMPLETED' | 'CANCELING' | 'CANCELED';

export interface PageInfo {
	hasNextPage: boolean;
	endCursor: string | null;
}

/** A connection as listed. */
export interface ExternalServiceSummary {
	id: string;
	kind: ExternalServiceKind;
	displayName: string;
	url: string;
	repoCount: number;
	lastSyncAt: string | null;
	nextSyncAt: string | null;
	lastSyncError: string | null;
	updatedAt: string;
	/** The resources file entry that owns the connection; such connections are read-only. */
	managedBy: string | null;
}

/** A connection with its (redacted) configuration and webhook settings. */
export interface ExternalServiceDetail extends ExternalServiceSummary {
	config: string;
	createdAt: string;
	webhookURL: string;
	webhookSecret: string | null;
	unrestricted: boolean;
	hasConnectionCheck: boolean;
	warning: string | null;
	rateLimiterState: RateLimiterState | null;
}

/** The connection's code host request budget, shared by every process. */
export interface RateLimiterState {
	burst: number;
	currentCapacity: number;
	infinite: boolean;
	interval: number;
	lastReplenishment: string;
	limit: number;
	pausedUntil: string | null;
}

/** A one-line summary of a request budget. */
export function rateLimitSummary(s: RateLimiterState): string {
	if (s.infinite) return 'Unlimited (rateLimit.enabled is false)';
	const perHour = Math.round((s.limit * 3600) / Math.max(1, s.interval));
	return `${perHour.toLocaleString('en-US')} requests/hour · ${s.currentCapacity.toLocaleString('en-US')} of ${s.burst.toLocaleString('en-US')} available now`;
}

export interface SyncJob {
	id: string;
	state: SyncJobState;
	reason: string;
	queuedAt: string;
	startedAt: string | null;
	finishedAt: string | null;
	failureMessage: string | null;
	warnings: string[];
	fullSync: boolean | null;
	reposSynced: number;
	reposAdded: number;
	reposModified: number;
	reposUnmodified: number;
	reposDeleted: number;
	repoSyncErrors: number;
}

export type Availability =
	| { __typename: 'ExternalServiceAvailable'; lastCheckedAt: string }
	| { __typename: 'ExternalServiceUnavailable'; suspectedReason: string }
	| {
			__typename: 'ExternalServiceAvailabilityUnknown';
			implementationNote: string;
	  };

const SUMMARY_FIELDS = `id kind displayName url repoCount lastSyncAt nextSyncAt lastSyncError updatedAt managedBy`;
const DETAIL_FIELDS = `${SUMMARY_FIELDS} config createdAt webhookURL webhookSecret unrestricted hasConnectionCheck warning
	rateLimiterState { burst currentCapacity infinite interval lastReplenishment limit pausedUntil }`;
const SYNC_JOB_FIELDS = `id state reason queuedAt startedAt finishedAt failureMessage warnings fullSync
	reposSynced reposAdded reposModified reposUnmodified reposDeleted repoSyncErrors`;

export const EXTERNAL_SERVICES = `query ExternalServices($first: Int, $after: String) {
	externalServices(first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { ${SUMMARY_FIELDS} }
	}
	${DECLARED_KINDS_FIELDS}
}`;
export type ExternalServicesResult = ExternalServicesQuery;

export const EXTERNAL_SERVICE = `query ExternalService($id: ID!, $first: Int, $after: String) {
	node(id: $id) {
		__typename
		... on ExternalService {
			${DETAIL_FIELDS}
			syncJobs(first: $first, after: $after) {
				totalCount
				pageInfo { hasNextPage endCursor }
				nodes { ${SYNC_JOB_FIELDS} }
			}
		}
	}
}`;
export type ExternalServiceResult = ExternalServiceQuery;

export const CHECK_CONNECTION = `query CheckExternalServiceConnection($id: ID!) {
	node(id: $id) {
		__typename
		... on ExternalService {
			checkConnection {
				__typename
				... on ExternalServiceAvailable { lastCheckedAt }
				... on ExternalServiceUnavailable { suspectedReason }
				... on ExternalServiceAvailabilityUnknown { implementationNote }
			}
		}
	}
}`;
export type CheckConnectionResult = CheckExternalServiceConnectionQuery;

export const VALIDATE_CONFIG = `query ValidateExternalServiceConfig($kind: ExternalServiceKind!, $config: String!) {
	validateExternalServiceConfig(kind: $kind, config: $config) { errors warnings }
}`;
export type ValidateConfigResult = ValidateExternalServiceConfigQuery;

export const CHECK_CONFIG = `query CheckExternalServiceConfig($kind: ExternalServiceKind!, $config: String!, $id: ID) {
	checkExternalServiceConfig(kind: $kind, config: $config, id: $id) { ok problem account scopes warnings }
}`;
export interface ConfigCheck {
	ok: boolean;
	problem: string | null;
	account: string | null;
	scopes: string[] | null;
	warnings: string[];
}
export type CheckConfigResult = CheckExternalServiceConfigQuery;

export const ADD_EXTERNAL_SERVICE = `mutation AddExternalService($input: AddExternalServiceInput!) {
	addExternalService(input: $input) { id warning }
}`;
export type AddExternalServiceResult = AddExternalServiceMutation;

export const UPDATE_EXTERNAL_SERVICE = `mutation UpdateExternalService($input: UpdateExternalServiceInput!) {
	updateExternalService(input: $input) { id warning }
}`;
export type UpdateExternalServiceResult = UpdateExternalServiceMutation;

export const DELETE_EXTERNAL_SERVICE = `mutation DeleteExternalService($id: ID!) {
	deleteExternalService(externalService: $id) { alwaysNil }
}`;

export const SYNC_EXTERNAL_SERVICE = `mutation SyncExternalService($id: ID!) {
	syncExternalService(id: $id) { alwaysNil }
}`;

export const CANCEL_EXTERNAL_SERVICE_SYNC = `mutation CancelExternalServiceSync($id: ID!) {
	cancelExternalServiceSync(id: $id) { alwaysNil }
}`;

export const REGENERATE_WEBHOOK_SECRET = `mutation RegenerateExternalServiceWebhookSecret($id: ID!) {
	regenerateExternalServiceWebhookSecret(id: $id) { webhookSecret externalService { id webhookSecret } }
}`;
export type RegenerateWebhookSecretResult = RegenerateExternalServiceWebhookSecretMutation;

/** The configuration a new GitLab connection starts with (fields of `gitlab.schema.json`). */
export const DEFAULT_GITLAB_CONFIG = `{
  // The GitLab instance: https://gitlab.com or a self-managed URL.
  "url": "https://gitlab.com",
  // A personal access token with the read_api scope (api for batch changes).
  "token": "<access token>",
  // Which projects to mirror: API paths with query parameters, e.g.
  // "groups/mygroup/projects" or "projects?membership=true&archived=no".
  "projectQuery": ["projects?membership=true&archived=no"]
}
`;

/** The configuration a new GitHub connection starts with. */
export const DEFAULT_GITHUB_CONFIG = `{
  // The GitHub instance: https://github.com or a GitHub Enterprise Server URL.
  "url": "https://github.com",
  // A classic token with the repo scope (reaches every organization of its user), or a
  // fine-grained token (Contents and Metadata: read), which reaches one resource owner only.
  "token": "<access token>",
  // Which repositories to mirror: "affiliated" (the token user's repositories and those of
  // its organizations), "none", or a repository search query. Organizations go in "orgs",
  // single repositories in "repos"; all of them are mirrored together.
  "repositoryQuery": ["affiliated"]
}
`;

/** The configuration a new Gitea connection starts with. */
export const DEFAULT_GITEA_CONFIG = `{
  // The Gitea instance, with its path if it is served under one.
  "url": "https://gitea.example.com",
  // An access token with the read:repository, read:organization and read:user scopes.
  "token": "<access token>",
  // Which repositories to mirror: "affiliated" (the token user's repositories), "all" (every
  // repository the token can read), "none", or a search keyword. Organizations go in "orgs",
  // single repositories in "repos".
  "repositoryQuery": ["affiliated"]
}
`;

/** The configuration a new connection of `kind` starts with. */
export function defaultConfig(kind: ExternalServiceKind): string {
	switch (kind) {
		case 'GITHUB':
			return DEFAULT_GITHUB_CONFIG;
		case 'GITEA':
			return DEFAULT_GITEA_CONFIG;
		default:
			return DEFAULT_GITLAB_CONFIG;
	}
}

/** Text of the states, as shown in the sync history. */
export const SYNC_STATE_LABELS: Record<SyncJobState, string> = {
	QUEUED: 'Queued',
	PROCESSING: 'Running',
	ERRORED: 'Errored (will retry)',
	FAILED: 'Failed',
	COMPLETED: 'Completed',
	CANCELING: 'Canceling',
	CANCELED: 'Canceled'
};

/** Whether a sync job can still be canceled. */
export function isCancelable(state: SyncJobState): boolean {
	return state === 'QUEUED' || state === 'PROCESSING';
}

/** Whether a sync job is queued or running (the page polls while one is). */
export function isActive(state: SyncJobState): boolean {
	return state === 'QUEUED' || state === 'PROCESSING' || state === 'CANCELING';
}

/** Why a sync was queued, as shown in the sync history. */
export function reasonLabel(reason: string): string {
	switch (reason) {
		case 'scheduled':
			return 'Scheduled';
		case 'manual':
			return 'Sync now';
		case 'config_changed':
			return 'Configuration changed';
		case 'webhook':
			return 'Webhook';
		default:
			return reason;
	}
}

/** "1m 05s" for the time between two timestamps, or null when either is missing. */
export function duration(start: string | null, end: string | null): string | null {
	if (!start || !end) return null;
	const ms = Date.parse(end) - Date.parse(start);
	if (!Number.isFinite(ms) || ms < 0) return null;
	const s = Math.round(ms / 1000);
	if (s < 60) return `${s}s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
	return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
}

/** The `messages` of a VALIDATION error, if the error is one. */
export function validationMessages(err: unknown): string[] | null {
	const errors = (err as { errors?: { extensions?: Record<string, unknown> }[] })?.errors;
	if (!Array.isArray(errors)) return null;
	for (const e of errors) {
		if (e.extensions?.['code'] !== 'VALIDATION') continue;
		const messages = e.extensions['messages'];
		if (Array.isArray(messages)) return messages.map(String);
	}
	return null;
}

/**
 * A notice for the connection page shown after navigating there from a save (the post-save
 * connection check's warning is only returned by the mutation).
 */
let flash: string | null = null;

/** Sets the notice the next connection page shows. */
export function setFlash(message: string | null): void {
	flash = message;
}

/** Returns and clears the pending notice. */
export function takeFlash(): string | null {
	const m = flash;
	flash = null;
	return m;
}
