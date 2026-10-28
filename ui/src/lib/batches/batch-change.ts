/**
 * Bulk changes: the list, the detail page's header, stats, close/delete, the burndown
 * chart and the bulk operations tab.
 */
import type {
	BatchChangeDetailQuery,
	BatchChangesQuery,
	BulkOperationsQuery,
	ChangesetCountsOverTimeQuery,
	OrgBatchChangesQuery,
	UserBatchChangesQuery
} from '#lib/graphql-types.ts';

// ---- Shared types ----------------------------------------------------------------------------

export type BatchChangeState = 'OPEN' | 'CLOSED' | 'DRAFT';

/** A user or organization namespace, as bulk change queries select it. */
export type NamespaceRef =
	| { __typename: 'User'; id: string; username: string; url: string }
	| { __typename: 'Org'; id: string; name: string; url: string };

const NAMESPACE_FIELDS = `__typename id url ... on User { username } ... on Org { name }`;

/** The namespace's name as shown (`alice`, `acme`). */
export function namespaceName(ns: NamespaceRef): string {
	return ns.__typename === 'User' ? ns.username : ns.name;
}

export interface ChangesetsStats {
	unpublished: number;
	draft: number;
	open: number;
	merged: number;
	closed: number;
	deleted: number;
	retrying: number;
	failed: number;
	scheduled: number;
	processing: number;
	archived: number;
	total: number;
	isCompleted: boolean;
	percentComplete: number;
}

const STATS_FIELDS = `unpublished draft open merged closed deleted retrying failed scheduled processing archived total isCompleted percentComplete`;

export interface DiffStat {
	added: number;
	deleted: number;
}

// ---- URLs ------------------------------------------------------------------------------------

/** The two namespace kinds in URLs: `/users/<name>`, `/organizations/<name>`. */
export type NamespaceKind = 'users' | 'organizations';

export function isNamespaceKind(s: string): s is NamespaceKind {
	return s === 'users' || s === 'organizations';
}

/** `/users/alice/bulk-changes` or `/organizations/acme/bulk-changes`. */
export function namespaceBatchChangesURL(kind: NamespaceKind, ns: string): string {
	return `/${kind}/${encodeURIComponent(ns)}/bulk-changes`;
}

/** A bulk change's page (the same as `BatchChange.url`). */
export function batchChangeURL(kind: NamespaceKind, ns: string, name: string): string {
	return `${namespaceBatchChangesURL(kind, ns)}/${encodeURIComponent(name)}`;
}

/** The detail page's tabs, in order, with their `?tab=` values. */
export const DETAIL_TABS = [
	{ id: 'changesets', title: 'Changesets' },
	{ id: 'chart', title: 'Burndown chart' },
	{ id: 'spec', title: 'Spec' },
	{ id: 'archived', title: 'Archived' },
	{ id: 'bulkoperations', title: 'Bulk operations' }
] as const;

export type DetailTab = (typeof DETAIL_TABS)[number]['id'];

/** The tab named by `?tab=`, the changesets tab by default. */
export function detailTab(param: string | null): DetailTab {
	return DETAIL_TABS.find((t) => t.id === param)?.id ?? 'changesets';
}

// ---- List ------------------------------------------------------------------------------------

const LIST_FIELDS = `
	id name description state url createdAt closedAt lastAppliedAt
	namespace { ${NAMESPACE_FIELDS} }
	creator { username }
	changesetsStats { ${STATS_FIELDS} }`;

export const BATCH_CHANGES = `query BatchChanges($first: Int, $after: String, $states: [BatchChangeState!]) {
	batchChanges(first: $first, after: $after, states: $states) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { ${LIST_FIELDS} }
	}
}`;

export const USER_BATCH_CHANGES = `query UserBatchChanges($user: String!, $first: Int, $after: String, $states: [BatchChangeState!]) {
	user(username: $user) {
		id
		batchChanges(first: $first, after: $after, states: $states) {
			totalCount
			pageInfo { hasNextPage endCursor }
			nodes { ${LIST_FIELDS} }
		}
	}
}`;

export const ORG_BATCH_CHANGES = `query OrgBatchChanges($org: String!, $first: Int, $after: String, $states: [BatchChangeState!]) {
	organization(name: $org) {
		id
		batchChanges(first: $first, after: $after, states: $states) {
			totalCount
			pageInfo { hasNextPage endCursor }
			nodes { ${LIST_FIELDS} }
		}
	}
}`;

export interface BatchChangeListNode {
	id: string;
	name: string;
	description: string | null;
	state: BatchChangeState;
	url: string;
	createdAt: string;
	closedAt: string | null;
	lastAppliedAt: string | null;
	namespace: NamespaceRef;
	creator: { username: string } | null;
	changesetsStats: ChangesetsStats;
}

export interface BatchChangeConnection {
	totalCount: number;
	pageInfo: { hasNextPage: boolean; endCursor: string | null };
	nodes: BatchChangeListNode[];
}

export type BatchChangesResult = BatchChangesQuery;

export type UserBatchChangesResult = UserBatchChangesQuery;
export type OrgBatchChangesResult = OrgBatchChangesQuery;

/** The list's state filter: "Open / Draft / Closed / All". */
export const STATE_FILTERS = [
	{ id: 'all', title: 'All', states: null },
	{ id: 'open', title: 'Open', states: ['OPEN'] },
	{ id: 'draft', title: 'Draft', states: ['DRAFT'] },
	{ id: 'closed', title: 'Closed', states: ['CLOSED'] }
] as const satisfies readonly { id: string; title: string; states: BatchChangeState[] | null }[];

export type StateFilter = (typeof STATE_FILTERS)[number]['id'];

export function stateFilterStates(id: string): BatchChangeState[] | null {
	const f = STATE_FILTERS.find((s) => s.id === id);
	return f?.states ? [...f.states] : null;
}

// ---- Detail ----------------------------------------------------------------------------------

export const USER_ID = `query BatchChangeUserID($name: String!) { user(username: $name) { id } }`;
export const ORG_ID = `query BatchChangeOrgID($name: String!) { organization(name: $name) { id } }`;

export const BATCH_CHANGE = `query BatchChangeDetail($namespace: ID!, $name: String!) {
	batchChange(namespace: $namespace, name: $name) {
		id name description state url viewerCanAdminister
		createdAt updatedAt lastAppliedAt closedAt
		namespace { ${NAMESPACE_FIELDS} }
		creator { username url }
		lastApplier { username url }
		diffStat { added deleted }
		changesetsStats { ${STATS_FIELDS} }
		currentSpec { id originalInput createdAt source applyURL }
		bulkOperations(first: 1) { totalCount }
	}
}`;

export interface BatchChangeDetail {
	id: string;
	name: string;
	description: string | null;
	state: BatchChangeState;
	url: string;
	viewerCanAdminister: boolean;
	createdAt: string;
	updatedAt: string;
	lastAppliedAt: string | null;
	closedAt: string | null;
	namespace: NamespaceRef;
	creator: { username: string; url: string } | null;
	lastApplier: { username: string; url: string } | null;
	diffStat: DiffStat;
	changesetsStats: ChangesetsStats;
	currentSpec: {
		id: string;
		originalInput: string;
		createdAt: string;
		source: 'LOCAL' | 'REMOTE';
		applyURL: string | null;
	};
	bulkOperations: { totalCount: number };
}

export type BatchChangeResult = BatchChangeDetailQuery;

export const CLOSE_BATCH_CHANGE = `mutation CloseBatchChange($batchChange: ID!, $closeChangesets: Boolean) {
	closeBatchChange(batchChange: $batchChange, closeChangesets: $closeChangesets) { id state closedAt }
}`;

export const DELETE_BATCH_CHANGE = `mutation DeleteBatchChange($batchChange: ID!) {
	deleteBatchChange(batchChange: $batchChange) { alwaysNil }
}`;

/** The edit page of the bulk change's spec (server-side specs; another part of the UI). */
export function editSpecURL(batchChangeUrl: string): string {
	return `${batchChangeUrl}/edit`;
}

/** The stats shown as the header's counters, in display order; zero counts are left out except open/merged/closed. */
export function statsEntries(s: ChangesetsStats): { key: string; label: string; count: number }[] {
	const all: { key: keyof ChangesetsStats; label: string; always?: boolean }[] = [
		{ key: 'processing', label: 'Processing' },
		{ key: 'scheduled', label: 'Scheduled' },
		{ key: 'retrying', label: 'Retrying' },
		{ key: 'failed', label: 'Failed' },
		{ key: 'unpublished', label: 'Unpublished' },
		{ key: 'draft', label: 'Draft' },
		{ key: 'open', label: 'Open', always: true },
		{ key: 'merged', label: 'Merged', always: true },
		{ key: 'closed', label: 'Closed', always: true },
		{ key: 'deleted', label: 'Deleted' },
		{ key: 'archived', label: 'Archived' }
	];
	return all
		.filter((e) => e.always || (s[e.key] as number) > 0)
		.map((e) => ({ key: e.key, label: e.label, count: s[e.key] as number }));
}

// ---- Burndown --------------------------------------------------------------------------------

export const CHANGESET_COUNTS_OVER_TIME = `query ChangesetCountsOverTime($batchChange: ID!, $from: DateTime, $includeArchived: Boolean) {
	node(id: $batchChange) {
		... on BatchChange {
			changesetCountsOverTime(from: $from, includeArchived: $includeArchived) {
				date total merged closed draft open openApproved openChangesRequested openPending
			}
		}
	}
}`;

export interface ChangesetCounts {
	date: string;
	total: number;
	merged: number;
	closed: number;
	draft: number;
	open: number;
	openApproved: number;
	openChangesRequested: number;
	openPending: number;
}

export type ChangesetCountsResult = ChangesetCountsOverTimeQuery;

/** The burndown's series in categorical slot order (the chart's palette validates this order). */
export const BURNDOWN_SERIES = [
	{ key: 'open', label: 'Open' },
	{ key: 'draft', label: 'Draft' },
	{ key: 'merged', label: 'Merged' },
	{ key: 'closed', label: 'Closed' }
] as const satisfies readonly { key: keyof ChangesetCounts; label: string }[];

export type BurndownKey = (typeof BURNDOWN_SERIES)[number]['key'];

/** Clean y-axis ticks from 0 to at least `max`: 1, 2 or 5 times a power of ten apart. */
export function niceTicks(max: number, target = 4): number[] {
	if (!(max > 0)) return [0, 1];
	const raw = max / target;
	const pow = 10 ** Math.floor(Math.log10(raw));
	const step = [1, 2, 5, 10].map((m) => m * pow).find((s) => s >= raw) ?? 10 * pow;
	const s = Math.max(1, step);
	const top = Math.ceil(max / s) * s;
	const out: number[] = [];
	for (let v = 0; v <= top + s / 2; v += s) out.push(v);
	return out;
}

/** Compact tick labels: 950, 1.2K, 12K, 3.4M. */
export function compactNumber(n: number): string {
	const abs = Math.abs(n);
	const fmt = (v: number, unit: string) => `${v < 10 && v % 1 !== 0 ? v.toFixed(1) : Math.round(v)}${unit}`;
	if (abs >= 1e6) return fmt(n / 1e6, 'M');
	if (abs >= 1e3) return fmt(n / 1e3, 'K');
	return String(n);
}

/** The `from` of a burndown range preset (`null`: the whole history). */
export function burndownFrom(range: string, now: Date = new Date()): string | null {
	const days = { '7d': 7, '30d': 30, '90d': 90 }[range];
	if (!days) return null;
	return new Date(now.getTime() - days * 86_400_000).toISOString();
}

// ---- Bulk operations -------------------------------------------------------------------------

export type BulkOperationType = 'COMMENT' | 'DETACH' | 'REENQUEUE' | 'MERGE' | 'CLOSE' | 'PUBLISH';
export type BulkOperationState = 'PROCESSING' | 'FAILED' | 'COMPLETED';

export const BULK_OPERATIONS = `query BulkOperations($batchChange: ID!, $first: Int, $after: String) {
	node(id: $batchChange) {
		... on BatchChange {
			bulkOperations(first: $first, after: $after) {
				totalCount
				pageInfo { hasNextPage endCursor }
				nodes {
					id type state progress changesetCount createdAt finishedAt
					initiator { username }
					errors {
						error
						changeset {
							__typename id
							... on ExternalChangeset { title repository { name } }
						}
					}
				}
			}
		}
	}
}`;

export interface BulkOperationNode {
	id: string;
	type: BulkOperationType;
	state: BulkOperationState;
	progress: number;
	changesetCount: number;
	createdAt: string;
	finishedAt: string | null;
	initiator: { username: string };
	errors: {
		error: string | null;
		changeset:
			| { __typename: 'ExternalChangeset'; id: string; title: string | null; repository: { name: string } }
			| { __typename: 'HiddenExternalChangeset'; id: string };
	}[];
}

export type BulkOperationsResult = BulkOperationsQuery;

/** What a bulk operation did, as a sentence: "Commented on 3 changesets". */
export function bulkOperationSummary(type: BulkOperationType, count: number): string {
	const n = `${count} ${count === 1 ? 'changeset' : 'changesets'}`;
	switch (type) {
		case 'COMMENT':
			return `Commented on ${n}`;
		case 'DETACH':
			return `Detached ${n}`;
		case 'REENQUEUE':
			return `Retried ${n}`;
		case 'MERGE':
			return `Merged ${n}`;
		case 'CLOSE':
			return `Closed ${n}`;
		case 'PUBLISH':
			return `Published ${n}`;
	}
}

/** Formats an ISO time for tables. */
export const formatDate = (iso: string): string => new Date(iso).toLocaleString();

/** "3 days ago", "in 2 hours". */
export function relativeTime(iso: string, now: Date = new Date()): string {
	const diff = new Date(iso).getTime() - now.getTime();
	const abs = Math.abs(diff);
	const units: [Intl.RelativeTimeFormatUnit, number][] = [
		['year', 365 * 86_400_000],
		['month', 30 * 86_400_000],
		['day', 86_400_000],
		['hour', 3_600_000],
		['minute', 60_000]
	];
	const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
	for (const [unit, ms] of units) {
		if (abs >= ms) return rtf.format(Math.round(diff / ms), unit);
	}
	return rtf.format(0, 'minute');
}
