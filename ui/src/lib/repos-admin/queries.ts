/**
 * GraphQL operations of the site admin repository pages: the repository
 * list with its status filters, one repository's mirror and text search index status, the
 * re-clone / re-index / fetch actions, and disk usage per repo node.
 */

import type { RemoteUnavailableReason } from '#lib/repo/queries.ts';
import type {
	AdminRepositoriesQuery,
	AdminRepositoryQuery,
	GitserversQuery,
	RepoRebalanceQuery
} from '#lib/graphql-types.ts';

export type RepositoryFilter =
	| 'all'
	| 'cloned'
	| 'cloning'
	| 'not-cloned'
	| 'failed'
	| 'indexed'
	| 'not-indexed'
	| 'corrupted';

export const REPOSITORY_FILTERS: { value: RepositoryFilter; label: string }[] = [
	{ value: 'all', label: 'All' },
	{ value: 'cloned', label: 'Cloned' },
	{ value: 'cloning', label: 'Cloning' },
	{ value: 'not-cloned', label: 'Not cloned' },
	{ value: 'failed', label: 'Failed fetch' },
	{ value: 'indexed', label: 'Indexed' },
	{ value: 'not-indexed', label: 'Not indexed' },
	{ value: 'corrupted', label: 'Corrupted' }
];

export interface AdminRepositoriesVariables {
	first: number;
	after: string | null;
	query: string | null;
	cloneStatus: 'CLONED' | 'CLONING' | 'NOT_CLONED' | null;
	failedFetch: boolean | null;
	corrupted: boolean | null;
	indexed: boolean | null;
	notIndexed: boolean | null;
}

/** A filter named in the page URL (`?filter=failed`), or `all`. */
export function parseFilter(value: string | null): RepositoryFilter {
	return REPOSITORY_FILTERS.find((f) => f.value === value)?.value ?? 'all';
}

/** The `repositories` arguments for a filter. */
export function filterVariables(
	filter: RepositoryFilter
): Omit<AdminRepositoriesVariables, 'first' | 'after' | 'query'> {
	const vars: Omit<AdminRepositoriesVariables, 'first' | 'after' | 'query'> = {
		cloneStatus: null,
		failedFetch: null,
		corrupted: null,
		indexed: null,
		notIndexed: null
	};
	switch (filter) {
		case 'cloned':
			vars.cloneStatus = 'CLONED';
			break;
		case 'cloning':
			vars.cloneStatus = 'CLONING';
			break;
		case 'not-cloned':
			vars.cloneStatus = 'NOT_CLONED';
			break;
		case 'failed':
			vars.failedFetch = true;
			break;
		case 'indexed':
			vars.notIndexed = false;
			break;
		case 'not-indexed':
			vars.indexed = false;
			break;
		case 'corrupted':
			vars.corrupted = true;
			break;
	}
	return vars;
}

const MIRROR_FIELDS = `cloned cloneInProgress cloneProgress isCorrupted updatedAt lastChanged lastError
	byteSize shard isEmpty remoteUnavailable`;

export const ADMIN_REPOSITORIES = `query AdminRepositories($first: Int, $after: String, $query: String,
	$cloneStatus: CloneStatus, $failedFetch: Boolean, $corrupted: Boolean, $indexed: Boolean,
	$notIndexed: Boolean) {
	repositoryStats {
		total cloned cloning notCloned queued failedFetch noCodeAccess indexed notIndexed corrupted
		gitDirBytes
	}
	statusMessages {
		__typename
		... on GitserverDiskThresholdReached { message address clonesPaused }
	}
	repositories(first: $first, after: $after, query: $query, cloneStatus: $cloneStatus,
		failedFetch: $failedFetch, corrupted: $corrupted, indexed: $indexed, notIndexed: $notIndexed) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id name url
			mirrorInfo { ${MIRROR_FIELDS} }
			textSearchIndex { lastIndexedAt lastError }
		}
	}
}`;

export interface MirrorInfo {
	cloned: boolean;
	cloneInProgress: boolean;
	cloneProgress: string | null;
	isCorrupted: boolean;
	updatedAt: string | null;
	lastChanged: string | null;
	lastError: string | null;
	byteSize: string;
	shard: string | null;
	isEmpty: boolean;
	remoteUnavailable: RemoteUnavailableReason | null;
}

export interface AdminRepositoryNode {
	id: string;
	name: string;
	url: string;
	mirrorInfo: MirrorInfo;
	textSearchIndex: { lastIndexedAt: string | null; lastError: string | null } | null;
}

export interface RepositoryStats {
	total: number;
	cloned: number;
	cloning: number;
	notCloned: number;
	/** Not cloned yet and no failed attempt. */
	queued: number;
	failedFetch: number;
	/** Cloned empty because the code host token may not download the code; not failures. */
	noCodeAccess: number;
	indexed: number;
	/** Cloned but not indexed yet. */
	notIndexed: number;
	corrupted: number;
	gitDirBytes: string;
}

export type AdminRepositoriesResult = AdminRepositoriesQuery;

export const ADMIN_REPOSITORY = `query AdminRepository($name: String!) {
	repository(name: $name) {
		id name url
		mirrorInfo { ${MIRROR_FIELDS} remoteURL nextSyncAt corruptionLogs { timestamp reason } }
		externalServices { nodes { id displayName } }
		textSearchIndex {
			status {
				updatedAt contentByteSize contentFilesCount indexByteSize indexShardsCount
				newLinesCount defaultBranchNewLinesCount otherBranchesNewLinesCount
			}
			refs { indexName ref { name displayName target { oid } } indexed current indexedCommit { oid } }
			host { name }
			lastIndexedAt lastError lastErrorAt queued nodeError
			shards { kind file byteSize filesCount repositoriesCount }
		}
	}
}`;

export interface IndexedRef {
	indexName: string;
	ref: { name: string; displayName: string; target: { oid: string } };
	indexed: boolean;
	current: boolean;
	indexedCommit: { oid: string } | null;
}

export interface TextSearchIndex {
	status: {
		updatedAt: string;
		contentByteSize: string;
		contentFilesCount: number;
		indexByteSize: number;
		indexShardsCount: number;
		newLinesCount: number;
		defaultBranchNewLinesCount: number;
		otherBranchesNewLinesCount: number;
	} | null;
	refs: IndexedRef[];
	host: { name: string } | null;
	lastIndexedAt: string | null;
	lastError: string | null;
	lastErrorAt: string | null;
	queued: boolean;
	nodeError: string | null;
	shards: {
		kind: string;
		file: string;
		byteSize: string;
		filesCount: number;
		repositoriesCount: number;
	}[];
}

export interface AdminRepository {
	id: string;
	name: string;
	url: string;
	mirrorInfo: MirrorInfo & {
		remoteURL: string;
		nextSyncAt: string | null;
		corruptionLogs: { timestamp: string; reason: string }[];
	};
	externalServices: { nodes: { id: string; displayName: string }[] };
	textSearchIndex: TextSearchIndex | null;
}

export type AdminRepositoryResult = AdminRepositoryQuery;

export const UPDATE_MIRROR_REPOSITORY = `mutation UpdateMirrorRepository($repository: ID!) {
	updateMirrorRepository(repository: $repository) { alwaysNil }
}`;

export const RECLONE_REPOSITORY = `mutation RecloneRepository($repo: ID!) {
	recloneRepository(repo: $repo) { alwaysNil }
}`;

export const REINDEX_REPOSITORY = `mutation ReindexRepository($repository: ID!) {
	reindexRepository(repository: $repository) { alwaysNil }
}`;

export const GITSERVERS = `query Gitservers {
	gitservers {
		totalCount
		nodes {
			id address freeDiskSpaceBytes totalDiskSpaceBytes configured error repositoriesCount
			assignedRepositoriesCount repositoriesDiskBytes repositoriesOnDisk
			indexedRepositoriesCount indexDiskBytes clonePauseThresholdBytes clonesPaused
		}
	}
}`;

export interface GitserverNode {
	id: string;
	address: string;
	freeDiskSpaceBytes: string;
	totalDiskSpaceBytes: string;
	configured: boolean;
	error: string | null;
	repositoriesCount: number;
	assignedRepositoriesCount: number;
	repositoriesDiskBytes: string;
	repositoriesOnDisk: number | null;
	indexedRepositoriesCount: number;
	indexDiskBytes: string | null;
	clonePauseThresholdBytes: string | null;
	clonesPaused: boolean;
}

export type GitserversResult = GitserversQuery;

export const REPO_REBALANCE = `query RepoRebalance {
	repoRebalance {
		waitingFor queued copying flipped failed done cancelled
		nodes { address nodeList alive configured lastSeenAt }
		moves(states: ["queued", "copying", "flipped", "failed"], first: 50) {
			repositoryName repositoryId fromNode toNode kind state attempts bytesCopied lastError
			nextAttemptAt startedAt flippedAt updatedAt
		}
	}
}`;

export const RETRY_FAILED_REPO_MOVES = `mutation RetryFailedRepoMoves { retryFailedRepoMoves }`;

export type RepoRebalanceResult = RepoRebalanceQuery;
export type RepoRebalance = RepoRebalanceQuery['repoRebalance'];

/** One line on where rebalancing stands. */
export function rebalanceSummary(r: RepoRebalance): string {
	if (r.waitingFor.length > 0) {
		return `Waiting for ${r.waitingFor.join(', ')} to run with the same node list before moving repositories.`;
	}
	const open = r.queued + r.copying + r.flipped;
	if (open === 0 && r.failed === 0) return 'Balanced: every repository is on its node.';
	const parts = [
		r.copying ? `${r.copying.toLocaleString()} copying` : '',
		r.queued ? `${r.queued.toLocaleString()} queued` : '',
		r.flipped ? `${r.flipped.toLocaleString()} switched over, old copy pending removal` : '',
		r.failed ? `${r.failed.toLocaleString()} failed` : ''
	].filter(Boolean);
	return `Moving repositories: ${parts.join(', ')}.`;
}

/** `1536` → `1.5 KiB`; accepts the string form of `BigInt`. */
export function formatBytes(value: string | number | null | undefined): string {
	const n = typeof value === 'string' ? Number(value) : (value ?? 0);
	if (!Number.isFinite(n) || n < 1024) return `${Number.isFinite(n) ? n : 0} B`;
	const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
	let v = n;
	let unit = 'B';
	for (const u of units) {
		if (v < 1024) break;
		v /= 1024;
		unit = u;
	}
	return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${unit}`;
}

/** `2026-01-01T00:00:00Z` → `3h ago` (or `in 5m` for the future); `—` for null. */
export function relativeTime(iso: string | null | undefined, now = Date.now()): string {
	if (!iso) return '—';
	const diff = Math.round((now - new Date(iso).getTime()) / 1000);
	const secs = Math.abs(diff);
	const text =
		secs < 60
			? `${secs}s`
			: secs < 3600
				? `${Math.round(secs / 60)}m`
				: secs < 86400
					? `${Math.round(secs / 3600)}h`
					: `${Math.round(secs / 86400)}d`;
	return diff >= 0 ? `${text} ago` : `in ${text}`;
}

/** The clone state of a repository as one word. */
export function cloneState(m: MirrorInfo): 'cloned' | 'cloning' | 'not cloned' {
	if (m.cloneInProgress) return 'cloning';
	return m.cloned ? 'cloned' : 'not cloned';
}

/**
 * The web page of a repository's remote: an http(s) clone URL without its `.git` suffix (and
 * without credentials), or an scp-style SSH remote (`git@host:path.git`, `ssh://git@host/path`)
 * turned into https. Null when the remote can't be mapped to a web page (e.g. a local path).
 */
export function remoteWebURL(remote: string): string | null {
	const scp = /^[\w.-]+@([\w.-]+):(?!\/)(.+)$/.exec(remote);
	const candidate = scp ? `https://${scp[1]}/${scp[2]}` : remote.replace(/^ssh:\/\/(?:[^@/]+@)?([^/:]+)(?::\d+)?\//, 'https://$1/');
	let url: URL;
	try {
		url = new URL(candidate);
	} catch {
		return null;
	}
	if (url.protocol !== 'https:' && url.protocol !== 'http:') return null;
	url.username = '';
	url.password = '';
	url.pathname = url.pathname.replace(/\.git\/?$/, '');
	return url.toString();
}
