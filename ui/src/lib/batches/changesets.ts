/**
 * The changesets of a bulk change: the filtered list, a changeset's diff from its spec,
 * selection with the available bulk operations, the bulk mutations and the CSV export.
 */
import type { BulkOperationType, DiffStat } from './batch-change.ts';
import type {
	AvailableBulkOperationsQuery,
	BatchChangeChangesetIDsQuery,
	BatchChangeChangesetsQuery,
	ChangesetsCsvQuery,
	ChangesetSpecDiffQuery
} from '#lib/graphql-types.ts';

export type ChangesetState =
	| 'UNPUBLISHED'
	| 'SCHEDULED'
	| 'PROCESSING'
	| 'RETRYING'
	| 'FAILED'
	| 'OPEN'
	| 'DRAFT'
	| 'CLOSED'
	| 'MERGED'
	| 'READONLY'
	| 'DELETED';
export type ChangesetReviewState = 'APPROVED' | 'CHANGES_REQUESTED' | 'PENDING' | 'COMMENTED' | 'DISMISSED';
export type ChangesetCheckState = 'PENDING' | 'PASSED' | 'FAILED';

export const CHANGESET_STATES: ChangesetState[] = [
	'OPEN',
	'DRAFT',
	'MERGED',
	'CLOSED',
	'UNPUBLISHED',
	'SCHEDULED',
	'PROCESSING',
	'RETRYING',
	'FAILED',
	'READONLY',
	'DELETED'
];
export const REVIEW_STATES: ChangesetReviewState[] = [
	'APPROVED',
	'CHANGES_REQUESTED',
	'PENDING',
	'COMMENTED',
	'DISMISSED'
];
export const CHECK_STATES: ChangesetCheckState[] = ['PENDING', 'PASSED', 'FAILED'];

/** `CHANGES_REQUESTED` → `Changes requested`. */
export function enumLabel(v: string): string {
	const s = v.toLowerCase().replace(/_/g, ' ');
	return s.charAt(0).toUpperCase() + s.slice(1);
}

/** The tone of a state's badge (CSS class suffix). */
export function stateTone(state: ChangesetState): 'success' | 'merged' | 'danger' | 'muted' | 'info' | 'warning' {
	switch (state) {
		case 'OPEN':
			return 'success';
		case 'MERGED':
			return 'merged';
		case 'CLOSED':
		case 'FAILED':
		case 'DELETED':
			return 'danger';
		case 'PROCESSING':
		case 'SCHEDULED':
			return 'info';
		case 'RETRYING':
		case 'READONLY':
			return 'warning';
		default:
			return 'muted';
	}
}

// ---- List ------------------------------------------------------------------------------------

/** The list's filters (state, review state, check state, search). */
export interface ChangesetFilters {
	state: ChangesetState | '';
	reviewState: ChangesetReviewState | '';
	checkState: ChangesetCheckState | '';
	search: string;
	onlyArchived: boolean;
}

export const EMPTY_FILTERS: ChangesetFilters = {
	state: '',
	reviewState: '',
	checkState: '',
	search: '',
	onlyArchived: false
};

export interface ChangesetFilterVariables {
	state: ChangesetState | null;
	reviewState: ChangesetReviewState | null;
	checkState: ChangesetCheckState | null;
	search: string | null;
	onlyArchived: boolean;
}

/** The GraphQL variables of the filters (empty ones as null). */
export function filterVariables(f: ChangesetFilters): ChangesetFilterVariables {
	const search = f.search.trim();
	return {
		state: f.state || null,
		reviewState: f.reviewState || null,
		checkState: f.checkState || null,
		search: search === '' ? null : search,
		onlyArchived: f.onlyArchived
	};
}

const FILTER_PARAMS = `$state: ChangesetState, $reviewState: ChangesetReviewState, $checkState: ChangesetCheckState, $search: String, $onlyArchived: Boolean`;
const FILTER_ARGS = `state: $state, reviewState: $reviewState, checkState: $checkState, search: $search, onlyArchived: $onlyArchived`;

export const CHANGESETS = `query BatchChangeChangesets($batchChange: ID!, $first: Int, $after: String, ${FILTER_PARAMS}) {
	node(id: $batchChange) {
		... on BatchChange {
			changesets(first: $first, after: $after, ${FILTER_ARGS}) {
				totalCount
				pageInfo { hasNextPage endCursor }
				nodes {
					__typename id state createdAt updatedAt nextSyncAt
					... on ExternalChangeset {
						title body externalID
						externalURL { url }
						repository { id name url }
						reviewState checkState
						diffStat { added deleted }
						error syncerError scheduleEstimateAt
						labels { text color }
						forkNamespace
						currentSpec {
							id
							description {
								__typename
								... on GitBranchChangesetDescription { baseRef headRef published }
							}
						}
					}
				}
			}
		}
	}
}`;

/** The IDs of every changeset matching the filters, for "select all". */
export const CHANGESET_IDS = `query BatchChangeChangesetIDs($batchChange: ID!, $first: Int, ${FILTER_PARAMS}) {
	node(id: $batchChange) {
		... on BatchChange {
			changesets(first: $first, ${FILTER_ARGS}) { totalCount nodes { __typename id } }
		}
	}
}`;

export interface ExternalChangesetNode {
	__typename: 'ExternalChangeset';
	id: string;
	state: ChangesetState;
	createdAt: string;
	updatedAt: string;
	nextSyncAt: string | null;
	title: string | null;
	body: string | null;
	externalID: string | null;
	externalURL: { url: string } | null;
	repository: { id: string; name: string; url: string };
	reviewState: ChangesetReviewState | null;
	checkState: ChangesetCheckState | null;
	diffStat: DiffStat | null;
	error: string | null;
	syncerError: string | null;
	scheduleEstimateAt: string | null;
	labels: { text: string; color: string }[];
	forkNamespace: string | null;
	currentSpec: {
		id: string;
		description:
			| { __typename: 'GitBranchChangesetDescription'; baseRef: string; headRef: string; published: unknown }
			| { __typename: 'ExistingChangesetReference' };
	} | null;
}

export interface HiddenChangesetNode {
	__typename: 'HiddenExternalChangeset';
	id: string;
	state: ChangesetState;
	createdAt: string;
	updatedAt: string;
	nextSyncAt: string | null;
}

export type ChangesetNode = ExternalChangesetNode | HiddenChangesetNode;

export type ChangesetsResult = BatchChangeChangesetsQuery;

export type ChangesetIDsResult = BatchChangeChangesetIDsQuery;

/** The changeset's branch (from its spec), if it has one. */
export function changesetBranch(c: ExternalChangesetNode): string | null {
	const d = c.currentSpec?.description;
	return d?.__typename === 'GitBranchChangesetDescription' ? d.headRef : null;
}

// ---- Diff ------------------------------------------------------------------------------------

export const CHANGESET_SPEC_DIFF = `query ChangesetSpecDiff($id: ID!, $first: Int) {
	node(id: $id) {
		... on VisibleChangesetSpec {
			description {
				__typename
				... on GitBranchChangesetDescription {
					diff {
						fileDiffs(first: $first) {
							totalCount
							diffStat { added deleted }
							nodes {
								oldPath newPath binary
								mostRelevantFile { __typename url }
								stat { added deleted }
								hunks { oldRange { startLine lines } newRange { startLine lines } body }
							}
						}
					}
				}
			}
		}
	}
}`;

export interface PreviewFileDiffNode {
	oldPath: string | null;
	newPath: string | null;
	binary: boolean;
	/** The new file (a `VirtualFile`, no page yet), or the old one at the base revision. */
	mostRelevantFile?: { __typename: string; url: string } | null;
	stat: DiffStat;
	hunks: {
		oldRange: { startLine: number; lines: number };
		newRange: { startLine: number; lines: number };
		body: string;
	}[];
}

export interface PreviewFileDiffs {
	totalCount: number;
	diffStat: DiffStat;
	nodes: PreviewFileDiffNode[];
}

export type ChangesetSpecDiffResult = ChangesetSpecDiffQuery;

/**
 * A spec's file diffs in the shape of `DiffView` (status from the paths; nothing is too large).
 * Only a file that exists at the base revision (a deleted one) links to
 * its page; new contents have none yet.
 */
export function toFileDiffConnection(d: PreviewFileDiffs) {
	return {
		totalCount: d.totalCount,
		diffStat: d.diffStat,
		nodes: d.nodes.map(({ mostRelevantFile, ...f }) => ({
			...f,
			url: mostRelevantFile?.__typename === 'GitBlob' ? mostRelevantFile.url : null,
			status:
				f.oldPath === null
					? 'ADDED'
					: f.newPath === null
						? 'DELETED'
						: f.oldPath !== f.newPath
							? 'RENAMED'
							: 'MODIFIED',
			tooLarge: false
		}))
	};
}

// ---- Selection and bulk operations -----------------------------------------------------------

export const AVAILABLE_BULK_OPERATIONS = `query AvailableBulkOperations($batchChange: ID!, $changesets: [ID!]!) {
	node(id: $batchChange) {
		... on BatchChange { availableBulkOperations(changesets: $changesets) }
	}
}`;

export type AvailableBulkOperationsResult = AvailableBulkOperationsQuery;

/** The bulk actions in menu order, with what each asks for. */
export const BULK_ACTIONS: {
	type: BulkOperationType;
	title: string;
	description: string;
	/** An extra input: a comment body, or a checkbox. */
	input?: 'body' | 'squash' | 'draft';
}[] = [
	{
		type: 'COMMENT',
		title: 'Comment',
		description: 'Create a comment on all selected changesets.',
		input: 'body'
	},
	{
		type: 'DETACH',
		title: 'Detach',
		description: 'Remove the selected archived changesets from this bulk change.'
	},
	{
		type: 'REENQUEUE',
		title: 'Retry',
		description: 'Re-enqueue the selected failed changesets for processing.'
	},
	{
		type: 'MERGE',
		title: 'Merge',
		description: 'Merge the selected open changesets on the code host.',
		input: 'squash'
	},
	{
		type: 'CLOSE',
		title: 'Close',
		description: 'Close the selected changesets on the code host.'
	},
	{
		type: 'PUBLISH',
		title: 'Publish',
		description: 'Publish the selected changesets whose spec does not set `published`.',
		input: 'draft'
	}
];

export const CREATE_CHANGESET_COMMENTS = `mutation CreateChangesetComments($batchChange: ID!, $changesets: [ID!]!, $body: String!) {
	createChangesetComments(batchChange: $batchChange, changesets: $changesets, body: $body) { id }
}`;
export const DETACH_CHANGESETS = `mutation DetachChangesets($batchChange: ID!, $changesets: [ID!]!) {
	detachChangesets(batchChange: $batchChange, changesets: $changesets) { id }
}`;
export const REENQUEUE_CHANGESETS = `mutation ReenqueueChangesets($batchChange: ID!, $changesets: [ID!]!) {
	reenqueueChangesets(batchChange: $batchChange, changesets: $changesets) { id }
}`;
export const MERGE_CHANGESETS = `mutation MergeChangesets($batchChange: ID!, $changesets: [ID!]!, $squash: Boolean) {
	mergeChangesets(batchChange: $batchChange, changesets: $changesets, squash: $squash) { id }
}`;
export const CLOSE_CHANGESETS = `mutation CloseChangesets($batchChange: ID!, $changesets: [ID!]!) {
	closeChangesets(batchChange: $batchChange, changesets: $changesets) { id }
}`;
export const PUBLISH_CHANGESETS = `mutation PublishChangesets($batchChange: ID!, $changesets: [ID!]!, $draft: Boolean) {
	publishChangesets(batchChange: $batchChange, changesets: $changesets, draft: $draft) { id }
}`;

/** The mutation and variables of a bulk action. */
export function bulkMutation(
	type: BulkOperationType,
	batchChange: string,
	changesets: string[],
	opts: { body?: string; squash?: boolean; draft?: boolean } = {}
): { query: string; variables: Record<string, unknown> } {
	const base = { batchChange, changesets };
	switch (type) {
		case 'COMMENT':
			return { query: CREATE_CHANGESET_COMMENTS, variables: { ...base, body: opts.body ?? '' } };
		case 'DETACH':
			return { query: DETACH_CHANGESETS, variables: base };
		case 'REENQUEUE':
			return { query: REENQUEUE_CHANGESETS, variables: base };
		case 'MERGE':
			return { query: MERGE_CHANGESETS, variables: { ...base, squash: opts.squash ?? false } };
		case 'CLOSE':
			return { query: CLOSE_CHANGESETS, variables: base };
		case 'PUBLISH':
			return { query: PUBLISH_CHANGESETS, variables: { ...base, draft: opts.draft ?? false } };
	}
}

// ---- CSV export ------------------------------------------------------------------------------

export const CHANGESETS_CSV = `query ChangesetsCSV($batchChange: ID!, $changesets: [ID!], ${FILTER_PARAMS}) {
	node(id: $batchChange) {
		... on BatchChange { changesetsCSV(changesets: $changesets, ${FILTER_ARGS}) }
	}
}`;

export type ChangesetsCSVResult = ChangesetsCsvQuery;

/** `<name>-changesets.csv`, with characters unsafe in file names replaced. */
export function csvFileName(batchChangeName: string): string {
	return `${batchChangeName.replace(/[^A-Za-z0-9._-]+/g, '-')}-changesets.csv`;
}
