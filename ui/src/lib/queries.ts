/**
 * GraphQL operations used by the UI, with their result and variable types.
 */
import {
	DECLARED_CONFIG_FILE_FIELDS,
	DECLARED_KINDS_FIELDS,
	type DeclaredConfigFile,
	type DeclaredKindsFile
} from './declared-config.ts';
import type {
	AccessRequestsQuery,
	CreateAccessTokenMutation,
	CurrentUserAccessTokensQuery,
	ExecutorSecretAccessLogsQuery,
	ExecutorsQuery,
	GlobalExecutorSecretsQuery,
	OrganizeSiteConfigurationQuery,
	PermissionsSubjectsQuery,
	PermissionsSyncJobsQuery,
	PreciseIndexCoverageQuery,
	RankingSummaryQuery,
	RbacQuery,
	RepositoryAccessExplanationQuery,
	SetAccessRequestStatusMutation,
	SiteConfigurationQuery,
	UpdateSiteConfigurationMutation,
	UserExecutorSecretsQuery,
	UsersQuery
} from '#lib/graphql-types.ts';

// ---- Site configuration ----------------------------------------------------------------------

export const SITE_CONFIGURATION = `query SiteConfiguration {
	site {
		id
		configuration {
			id
			effectiveContents
			validationMessages
			readOnly
		}
		declaredConfigFiles { ${DECLARED_CONFIG_FILE_FIELDS} }
	}
}`;

export type SiteConfigurationResult = SiteConfigurationQuery;

export const UPDATE_SITE_CONFIGURATION = `mutation UpdateSiteConfiguration($lastID: Int!, $input: String!) {
	updateSiteConfiguration(lastID: $lastID, input: $input)
}`;

export type UpdateSiteConfigurationResult = UpdateSiteConfigurationMutation;

export interface UpdateSiteConfigurationVariables {
	lastID: number;
	input: string;
}

/** Reorders a site configuration document by section (nothing is saved). */
export const ORGANIZE_SITE_CONFIGURATION = `query OrganizeSiteConfiguration($input: String!) {
	site { organizeConfiguration(input: $input) }
}`;

export type OrganizeSiteConfigurationResult = OrganizeSiteConfigurationQuery;

// ---- Users -----------------------------------------------------------------------------------

export const USERS = `query Users($first: Int, $query: String, $includeDeactivated: Boolean) {
	users(first: $first, query: $query, includeDeactivated: $includeDeactivated) {
		nodes {
			id
			username
			displayName
			siteAdmin
			scimControlled
			createdAt
			deactivatedAt
			roles(first: 1000) { nodes { id name system } }
		}
		totalCount
	}
}`;

export interface UserNode {
	id: string;
	username: string;
	displayName: string | null;
	siteAdmin: boolean;
	scimControlled: boolean;
	createdAt: string;
	deactivatedAt: string | null;
	roles: { nodes: RoleSummary[] };
}

export interface RoleSummary { id: string; name: string; system: boolean }

export const RBAC = `query Rbac {
	roles(first: 1000) { nodes { id name system createdAt managedBy permissions(first: 100) { nodes { id namespace action displayName } } } }
	permissions(first: 100) { nodes { id namespace action displayName } }
	${DECLARED_KINDS_FIELDS}
}`;

export interface PermissionNode { id: string; namespace: string; action: string; displayName: string }
/** A role with its permissions; `managedBy` names the resources file entry that owns a custom role. */
export interface RoleNode extends RoleSummary { createdAt: string; managedBy: string | null; permissions: { nodes: PermissionNode[] } }
export type RbacResult = RbacQuery;

export const CREATE_ROLE = `mutation CreateRole($name: String!, $permissions: [ID!]!) {
	createRole(name: $name, permissions: $permissions) { id }
}`;
export const DELETE_ROLE = `mutation DeleteRole($role: ID!) { deleteRole(role: $role) { alwaysNil } }`;
export const SET_PERMISSIONS = `mutation SetPermissions($role: ID!, $permissions: [ID!]!) {
	setPermissions(role: $role, permissions: $permissions) { alwaysNil }
}`;
export const SET_ROLES = `mutation SetRoles($user: ID!, $roles: [ID!]!) {
	setRoles(user: $user, roles: $roles) { alwaysNil }
}`;

export type UsersResult = UsersQuery;

export interface UsersVariables {
	first?: number;
	query?: string | null;
	includeDeactivated?: boolean;
}

export const SET_USER_IS_SITE_ADMIN = `mutation SetUserIsSiteAdmin($userID: ID!, $siteAdmin: Boolean!) {
	setUserIsSiteAdmin(userID: $userID, siteAdmin: $siteAdmin) {
		alwaysNil
	}
}`;

export interface SetUserIsSiteAdminVariables {
	userID: string;
	siteAdmin: boolean;
}

// ---- Current user and access tokens ----------------------------------------------------------

export const CURRENT_USER_ACCESS_TOKENS = `query CurrentUserAccessTokens {
	currentUser {
		id
		username
		siteAdmin
		accessTokens {
			nodes {
				id
				note
				scopes
				createdAt
				lastUsedAt
				expiresAt
			}
		}
	}
}`;

export interface AccessTokenNode {
	id: string;
	note: string;
	scopes: string[];
	createdAt: string;
	lastUsedAt: string | null;
	expiresAt: string | null;
}

export type CurrentUserAccessTokensResult = CurrentUserAccessTokensQuery;

export const CREATE_ACCESS_TOKEN = `mutation CreateAccessToken($user: ID!, $scopes: [String!]!, $note: String!) {
	createAccessToken(user: $user, scopes: $scopes, note: $note) {
		id
		token
	}
}`;

export type CreateAccessTokenResult = CreateAccessTokenMutation;

export interface CreateAccessTokenVariables {
	user: string;
	scopes: string[];
	note: string;
}

export const DELETE_ACCESS_TOKEN = `mutation DeleteAccessToken($byID: ID!) {
	deleteAccessToken(byID: $byID) {
		alwaysNil
	}
}`;

export interface DeleteAccessTokenVariables {
	byID: string;
}

/** Access token scopes understood by the server. */
export const SCOPE_USER_ALL = 'user:all';
export const SCOPE_SITE_ADMIN_SUDO = 'site-admin:sudo';

// Repository permissions admin page.

export const PERMISSIONS_SUBJECTS = `query PermissionsSubjects($username: String!, $repo: String!) {
	user(username: $username) {
		id
		username
		permissionsInfo {
			syncedAt
		}
	}
	repository(name: $repo) {
		id
		name
	}
}`;

export type PermissionsSubjectsResult = PermissionsSubjectsQuery;

export interface PermissionsSubjectsVariables {
	username: string;
	repo: string;
}

export const REPOSITORY_ACCESS_EXPLANATION = `query RepositoryAccessExplanation($user: ID!, $repository: ID!) {
	repositoryAccessExplanation(user: $user, repository: $repository) {
		canRead
		summary
		reasons {
			kind
			detail
			grantedAt
		}
		userSyncedAt
		repositorySyncedAt
	}
}`;

export interface RepositoryAccessExplanation {
	canRead: boolean;
	summary: string;
	reasons: { kind: string; detail: string; grantedAt: string | null }[];
	userSyncedAt: string | null;
	repositorySyncedAt: string | null;
}

export type RepositoryAccessExplanationResult = RepositoryAccessExplanationQuery;

export interface RepositoryAccessExplanationVariables {
	user: string;
	repository: string;
}

export const PERMISSIONS_SYNC_JOBS = `query PermissionsSyncJobs($first: Int, $after: String, $state: PermissionsSyncJobState) {
	permissionsSyncJobs(first: $first, after: $after, state: $state) {
		nodes {
			id
			state
			reason
			queuedAt
			finishedAt
			ranForMs
			failureMessage
			permissionsAdded
			permissionsRemoved
			permissionsFound
			codeHostStates {
				providerID
				providerType
				status
				message
			}
			subject {
				__typename
				... on User {
					username
				}
				... on Repository {
					name
				}
			}
		}
		totalCount
		pageInfo {
			hasNextPage
			endCursor
		}
	}
}`;

export type PermissionsSyncJobState =
	| 'QUEUED'
	| 'PROCESSING'
	| 'ERRORED'
	| 'FAILED'
	| 'COMPLETED'
	| 'CANCELED';

export interface PermissionsSyncJobNode {
	id: string;
	state: PermissionsSyncJobState;
	reason: string;
	queuedAt: string;
	finishedAt: string | null;
	ranForMs: number | null;
	failureMessage: string | null;
	permissionsAdded: number;
	permissionsRemoved: number;
	permissionsFound: number;
	codeHostStates: { providerID: string; providerType: string; status: string; message: string }[];
	subject:
		| { __typename: 'User'; username: string }
		| { __typename: 'Repository'; name: string }
		| null;
}

export type PermissionsSyncJobsResult = PermissionsSyncJobsQuery;

export interface PermissionsSyncJobsVariables {
	first: number;
	after?: string | null;
	state?: PermissionsSyncJobState | null;
}

export const SCHEDULE_USER_PERMISSIONS_SYNC = `mutation ScheduleUserPermissionsSync($user: ID!) {
	scheduleUserPermissionsSync(user: $user) {
		alwaysNil
	}
}`;

export const SCHEDULE_REPOSITORY_PERMISSIONS_SYNC = `mutation ScheduleRepositoryPermissionsSync($repository: ID!) {
	scheduleRepositoryPermissionsSync(repository: $repository) {
		alwaysNil
	}
}`;

export const CANCEL_PERMISSIONS_SYNC_JOB = `mutation CancelPermissionsSyncJob($job: ID!) {
	cancelPermissionsSyncJob(job: $job) {
		alwaysNil
	}
}`;
// Access requests admin page.

export const ACCESS_REQUESTS = `query AccessRequests($first: Int) {
	accessRequests(status: PENDING, first: $first) {
		nodes {
			id
			name
			email
			additionalInfo
			createdAt
		}
		totalCount
	}
}`;

export interface AccessRequestNode {
	id: string;
	name: string;
	email: string;
	additionalInfo: string;
	createdAt: string;
}

export type AccessRequestsResult = AccessRequestsQuery;

export const EXECUTORS = `query Executors($query: String, $active: Boolean, $first: Int) {
	areExecutorsConfigured
	executors(query: $query, active: $active, first: $first) {
		nodes {
			id
			hostname
			queueNames
			active
			os
			architecture
			executorVersion
			runtime
			runtimeVersion
			gitVersion
			compatibility
			firstSeenAt
			lastSeenAt
			activeJobs {
				queue
				id
			}
		}
		totalCount
	}
}`;

export interface ExecutorNode {
	id: string;
	hostname: string;
	queueNames: string[];
	active: boolean;
	os: string;
	architecture: string;
	executorVersion: string;
	runtime: string;
	runtimeVersion: string;
	gitVersion: string;
	compatibility: 'UP_TO_DATE' | 'OUTDATED' | 'TOO_NEW' | null;
	firstSeenAt: string;
	lastSeenAt: string;
	activeJobs: { queue: string; id: number }[];
}

export type ExecutorsResult = ExecutorsQuery;

export const SET_ACCESS_REQUEST_STATUS = `mutation SetAccessRequestStatus($id: ID!, $status: AccessRequestStatus!) {
	setAccessRequestStatus(id: $id, status: $status) {
		resetUrl
	}
}`;

export type SetAccessRequestStatusResult = SetAccessRequestStatusMutation;

export interface SetAccessRequestStatusVariables {
	id: string;
	status: 'APPROVED' | 'REJECTED';
}

export const RANKING_SUMMARY = `query RankingSummary {
	rankingSummary {
		derivativeGraphKey
		enabled
		numExportedIndexes
		numRankedRepositories
		numRankedPaths
		rankingSummary {
			graphKey
			visibleToSearch
			startedAt
			completedAt
			failureMessage
		}
	}
}`;

export interface RankingRunSummary {
	graphKey: string;
	visibleToSearch: boolean;
	startedAt: string;
	completedAt: string | null;
	failureMessage: string | null;
}

export type RankingSummaryResult = RankingSummaryQuery;

/** Precise index coverage states (`PreciseIndexCoverageState`). */
export type PreciseIndexCoverageState = 'UP_TO_DATE' | 'STALE' | 'FAILING' | 'NONE' | 'NOT_TARGETED';

export const PRECISE_INDEX_COVERAGE = `query PreciseIndexCoverage(
	$staleAfterDays: Int!
	$state: PreciseIndexCoverageState
	$query: String
	$first: Int!
	$after: String
) {
	preciseIndexCoverage(staleAfterDays: $staleAfterDays) {
		staleAfterDays
		autoIndexingEnabled
		counts {
			upToDate
			stale
			failing
			none
			notTargeted
			total
		}
		repositories(state: $state, query: $query, first: $first, after: $after) {
			totalCount
			pageInfo {
				hasNextPage
				endCursor
			}
			nodes {
				repositoryID
				repositoryName
				codeGraphURL
				state
				headCommit
				indexedCommit
				indexedAt
				indexedAtHead
				failureMessage
				failedAt
				failureSource
			}
		}
	}
}`;

export interface PreciseIndexCoverageNode {
	repositoryID: string;
	repositoryName: string;
	codeGraphURL: string;
	state: PreciseIndexCoverageState;
	headCommit: string | null;
	indexedCommit: string | null;
	indexedAt: string | null;
	indexedAtHead: boolean;
	failureMessage: string | null;
	failedAt: string | null;
	failureSource: string | null;
}

export type PreciseIndexCoverageResult = PreciseIndexCoverageQuery;

export const BUMP_DERIVATIVE_GRAPH_KEY = `mutation BumpDerivativeGraphKey {
	bumpDerivativeGraphKey {
		alwaysNil
	}
}`;

/** Executor secret scopes: bulk changes and auto-indexing jobs. */
export type ExecutorSecretScope = 'BATCHES' | 'CODEINTEL';

const EXECUTOR_SECRET_FIELDS = `
	id
	key
	scope
	overwritesGlobalSecret
	namespace {
		id
		... on User { username }
		... on Org { name }
	}
	creator { username }
	createdAt
	updatedAt
	managedBy
	accessLogs(first: 0) { totalCount }`;

export interface ExecutorSecretNode {
	id: string;
	key: string;
	scope: ExecutorSecretScope;
	overwritesGlobalSecret: boolean;
	namespace: { id: string; username?: string; name?: string } | null;
	creator: { username: string } | null;
	createdAt: string;
	updatedAt: string;
	/** The resources file entry that owns this global secret. */
	managedBy: string | null;
	accessLogs: { totalCount: number };
}

export const GLOBAL_EXECUTOR_SECRETS = `query GlobalExecutorSecrets($scope: ExecutorSecretScope!, $first: Int) {
	executorSecrets(scope: $scope, first: $first) {
		nodes {${EXECUTOR_SECRET_FIELDS}
		}
		totalCount
	}
	${DECLARED_KINDS_FIELDS}
}`;

export type GlobalExecutorSecretsResult = GlobalExecutorSecretsQuery;

export const USER_EXECUTOR_SECRETS = `query UserExecutorSecrets($scope: ExecutorSecretScope!, $first: Int) {
	currentUser {
		id
		executorSecrets(scope: $scope, first: $first) {
			nodes {${EXECUTOR_SECRET_FIELDS}
			}
			totalCount
		}
	}
}`;

export type UserExecutorSecretsResult = UserExecutorSecretsQuery;

export const CREATE_EXECUTOR_SECRET = `mutation CreateExecutorSecret($scope: ExecutorSecretScope!, $key: String!, $value: String!, $namespace: ID) {
	createExecutorSecret(scope: $scope, key: $key, value: $value, namespace: $namespace) {
		id
	}
}`;

export interface CreateExecutorSecretVariables {
	scope: ExecutorSecretScope;
	key: string;
	value: string;
	namespace: string | null;
}

export const UPDATE_EXECUTOR_SECRET = `mutation UpdateExecutorSecret($scope: ExecutorSecretScope!, $id: ID!, $value: String!) {
	updateExecutorSecret(scope: $scope, id: $id, value: $value) {
		id
	}
}`;

export const DELETE_EXECUTOR_SECRET = `mutation DeleteExecutorSecret($scope: ExecutorSecretScope!, $id: ID!) {
	deleteExecutorSecret(scope: $scope, id: $id) {
		alwaysNil
	}
}`;

export const EXECUTOR_SECRET_ACCESS_LOGS = `query ExecutorSecretAccessLogs($id: ID!, $first: Int) {
	node(id: $id) {
		... on ExecutorSecret {
			accessLogs(first: $first) {
				nodes {
					id
					user { username }
					machineUser
					createdAt
				}
				totalCount
			}
		}
	}
}`;

export interface ExecutorSecretAccessLogNode {
	id: string;
	user: { username: string } | null;
	machineUser: string;
	createdAt: string;
}

export type ExecutorSecretAccessLogsResult = ExecutorSecretAccessLogsQuery;
