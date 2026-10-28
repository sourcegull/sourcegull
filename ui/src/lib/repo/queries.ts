/** GraphQL operations of the repository pages, with result types. */

import { graphql, GraphQLRequestError } from '#lib/graphql.ts';
import type {
	BlobQuery,
	CommitQuery,
	CompareQuery,
	GitRefsQuery,
	HistoryQuery,
	RepositoriesQuery,
	RepositoryHeaderQuery,
	TreeEntriesQuery
} from '#lib/graphql-types.ts';

export interface Signature {
	person: { name: string; email: string };
	date: string;
}

export interface CommitSummary {
	oid: string;
	abbreviatedOID: string;
	subject: string;
	url: string;
	author: Signature;
}

export interface PageInfo {
	hasNextPage: boolean;
	endCursor: string | null;
}

/** A user-facing message for repo node error codes, or null for other errors. */
export function cloneStateMessage(err: unknown): string | null {
	if (!(err instanceof GraphQLRequestError)) return null;
	if (err.hasCode('CLONE_IN_PROGRESS')) return 'This repository is being cloned. Try again in a moment.';
	if (err.hasCode('REPO_NOT_CLONED'))
		return 'This repository has not been cloned yet. It is queued and will be available soon.';
	if (err.hasCode('UNAVAILABLE')) return 'The repository server is unavailable. Try again later.';
	return null;
}

/** Why the code host has no repository Sourcegull can fetch (`mirrorInfo.remoteUnavailable`). */
export type RemoteUnavailableReason = 'MISSING' | 'DISABLED' | 'NO_CODE_ACCESS';

/** The emptiness fields of `mirrorInfo`. */
export interface MirrorEmptiness {
	isEmpty: boolean;
	remoteUnavailable: RemoteUnavailableReason | null;
}

/** The hint shown for repositories the code host token may not download. */
export const NO_CODE_ACCESS_HINT = 'The code host token needs at least Reporter access.';

/**
 * The notice for a cloned repository without commits, or null when it has some. Not an error:
 * GitLab projects can exist without a repository (never pushed to) or with the repository
 * feature disabled, the token may lack access to the code, and git repositories can have no
 * branch yet. The repo node picks the repository up on a later fetch.
 */
export function emptyRepoMessage(m: MirrorEmptiness): string | null {
	switch (m.remoteUnavailable) {
		case 'MISSING':
			return 'This repository is empty (no repository on GitLab yet).';
		case 'DISABLED':
			return 'Repository is disabled for this project on GitLab.';
		case 'NO_CODE_ACCESS':
			return `Sourcegull may not download this repository's code. ${NO_CODE_ACCESS_HINT}`;
	}
	if (m.isEmpty) return 'This repository is empty (no commits yet).';
	return null;
}

/** The status badge of an empty repository in the site admin list, or null when it has commits. */
export function emptyRepoBadge(
	m: MirrorEmptiness
): { label: string; title: string; warning: boolean } | null {
	switch (m.remoteUnavailable) {
		case 'MISSING':
			return { label: 'empty', title: 'No repository on GitLab yet', warning: false };
		case 'DISABLED':
			return {
				label: 'repository disabled',
				title: 'Repository is disabled for this project on GitLab',
				warning: false
			};
		case 'NO_CODE_ACCESS':
			return { label: 'no code access', title: NO_CODE_ACCESS_HINT, warning: true };
	}
	if (m.isEmpty) return { label: 'empty', title: 'No commits yet', warning: false };
	return null;
}

// ---- Repositories ---------------------------------------------------------------------------

export const REPOSITORIES = `query Repositories($first: Int!, $after: String, $query: String) {
	repositories(first: $first, after: $after, query: $query) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id name url description isPrivate isFork isArchived
			mirrorInfo { cloned cloneInProgress lastError }
		}
	}
}`;

export interface RepositoryListNode {
	id: string;
	name: string;
	url: string;
	description: string;
	isPrivate: boolean;
	isFork: boolean;
	isArchived: boolean;
	mirrorInfo: { cloned: boolean; cloneInProgress: boolean; lastError: string | null };
}

export type RepositoriesResult = RepositoriesQuery;

// ---- Repository header ----------------------------------------------------------------------

export const REPOSITORY_HEADER = `query RepositoryHeader($name: String!) {
	repository(name: $name) {
		id name url description
		externalURLs { url serviceKind }
		mirrorInfo { cloned cloneInProgress isEmpty remoteUnavailable }
		defaultBranch { displayName }
	}
}`;

export interface RepositoryHeader {
	id: string;
	name: string;
	url: string;
	description: string;
	externalURLs: { url: string; serviceKind: string | null }[];
	mirrorInfo: { cloned: boolean; cloneInProgress: boolean } & MirrorEmptiness;
	defaultBranch: { displayName: string } | null;
}

export async function loadRepositoryHeader(name: string): Promise<RepositoryHeader | null> {
	const r = await graphql<RepositoryHeaderQuery, { name: string }>(
		REPOSITORY_HEADER,
		{ name }
	);
	return r.repository;
}

// ---- Trees ----------------------------------------------------------------------------------

export interface TreeEntryNode {
	__typename: 'GitTree' | 'GitBlob';
	name: string;
	path: string;
	isDirectory: boolean;
	kind: 'FILE' | 'EXECUTABLE' | 'SYMLINK' | 'DIRECTORY' | 'SUBMODULE';
	url: string;
}

export const TREE_ENTRIES = `query TreeEntries($repo: String!, $rev: String!, $path: String!) {
	repository(name: $repo) {
		commit(rev: $rev) {
			oid
			tree(path: $path) {
				url
				archiveURL
				entries { __typename name path isDirectory kind url }
			}
		}
	}
}`;

export type TreeEntriesResult = TreeEntriesQuery;

export type TreeEntriesVariables = { repo: string; rev: string; path: string };

const treeCache = new Map<string, Promise<TreeEntriesResult>>();

/** Loads a directory listing, memoized for the session (keyed by repo, revision and path). */
export function loadTree(repo: string, rev: string, path: string): Promise<TreeEntriesResult> {
	const key = `${repo}\u0000${rev}\u0000${path}`;
	let p = treeCache.get(key);
	if (!p) {
		p = graphql<TreeEntriesResult, TreeEntriesVariables>(TREE_ENTRIES, { repo, rev, path });
		p.catch(() => treeCache.delete(key));
		treeCache.set(key, p);
	}
	return p;
}

// ---- Files ----------------------------------------------------------------------------------

export const BLOB = `query Blob($repo: String!, $rev: String!, $path: String!) {
	repository(name: $repo) {
		commit(rev: $rev) {
			oid
			blob(path: $path) {
				path name url canonicalURL rawURL byteSize binary contentTruncated totalLines languages
				content
				highlight { aborted packed language plainReason }
			}
			tree(path: $path) { url }
		}
	}
}`;

export interface BlobNode {
	path: string;
	name: string;
	url: string;
	canonicalURL: string;
	rawURL: string;
	byteSize: number;
	binary: boolean;
	contentTruncated: boolean;
	totalLines: number;
	languages: string[];
	content: string;
	highlight: { aborted: boolean; packed: number[]; language: string | null; plainReason: string | null };
}

export type BlobResult = BlobQuery;

// ---- Commits --------------------------------------------------------------------------------

const FILE_DIFF_FIELDS = `
	totalCount
	diffStat { added deleted }
	nodes {
		oldPath newPath status binary tooLarge
		stat { added deleted }
		hunks { oldRange { startLine lines } newRange { startLine lines } body }
	}`;

export interface FileDiffNode {
	oldPath: string | null;
	newPath: string | null;
	status: string;
	binary: boolean;
	tooLarge: boolean;
	stat: { added: number; deleted: number };
	hunks: {
		oldRange: { startLine: number; lines: number };
		newRange: { startLine: number; lines: number };
		body: string;
	}[];
	/** The file's page, when the diff has its own (a changeset spec's deleted file). */
	url?: string | null;
}

export interface FileDiffConnection {
	totalCount: number;
	diffStat: { added: number; deleted: number };
	nodes: FileDiffNode[];
}

export const COMMIT = `query Commit($repo: String!, $rev: String!) {
	repository(name: $repo) {
		commit(rev: $rev) {
			oid abbreviatedOID subject body url
			author { person { name email } date }
			committer { person { name email } date }
			parents { oid abbreviatedOID url }
			tree { url }
			diff { fileDiffs(first: 500) { ${FILE_DIFF_FIELDS} } }
		}
	}
}`;

export interface CommitDetail extends Omit<CommitSummary, 'author'> {
	body: string | null;
	author: Signature;
	committer: Signature | null;
	parents: { oid: string; abbreviatedOID: string; url: string }[];
	tree: { url: string } | null;
	diff: { fileDiffs: FileDiffConnection };
}

export type CommitResult = CommitQuery;

export const HISTORY = `query History($repo: String!, $rev: String!, $path: String, $first: Int!, $after: String) {
	repository(name: $repo) {
		commit(rev: $rev) {
			ancestors(first: $first, path: $path, afterCursor: $after) {
				nodes { oid abbreviatedOID subject url author { person { name email } date } }
				pageInfo { hasNextPage endCursor }
			}
		}
	}
}`;

export type HistoryResult = HistoryQuery;

// ---- Compare --------------------------------------------------------------------------------

export const COMPARE = `query Compare($repo: String!, $base: String, $head: String) {
	repository(name: $repo) {
		comparison(base: $base, head: $head) {
			range { expr mergeBase { oid abbreviatedOID } }
			commits(first: 250) {
				nodes { oid abbreviatedOID subject url author { person { name email } date } }
				truncated
			}
			fileDiffs(first: 500) { ${FILE_DIFF_FIELDS} }
		}
	}
}`;

export type CompareResult = CompareQuery;

// ---- Refs -----------------------------------------------------------------------------------

export type GitRefType = 'GIT_BRANCH' | 'GIT_TAG';

export const GIT_REFS = `query GitRefs($repo: String!, $type: GitRefType, $first: Int!, $query: String, $order: GitRefOrder!) {
	repository(name: $repo) {
		defaultBranch { name }
		gitRefs(type: $type, first: $first, query: $query, orderBy: $order) {
			totalCount
			nodes {
				name displayName type url
				target { oid abbreviatedOID commit { subject author { person { name } date } } }
			}
		}
	}
}`;

export interface GitRefNode {
	name: string;
	displayName: string;
	type: 'GIT_BRANCH' | 'GIT_TAG' | 'GIT_REF_OTHER';
	url: string;
	target: {
		oid: string;
		abbreviatedOID: string;
		commit: { subject: string; author: { person: { name: string }; date: string } } | null;
	};
}

export type GitRefsResult = GitRefsQuery;

export interface GitRefsVariables {
	repo: string;
	type: GitRefType | null;
	first: number;
	query: string | null;
	order: 'AUTHORED_OR_COMMITTED_AT' | 'ALPHABETICAL';
}
