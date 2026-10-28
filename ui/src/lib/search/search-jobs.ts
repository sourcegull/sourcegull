/** GraphQL operations and helpers of Search Jobs. */
import type { SearchJobsQuery } from '#lib/graphql-types.ts';

export type SearchJobState = 'QUEUED' | 'PROCESSING' | 'ERRORED' | 'FAILED' | 'COMPLETED' | 'CANCELED';

export interface SearchJobNode {
	id: string;
	query: string;
	state: SearchJobState;
	createdAt: string;
	startedAt: string | null;
	finishedAt: string | null;
	URL: string | null;
	logURL: string | null;
	creator: { username: string } | null;
	repoStats: { total: number; completed: number; failed: number; inProgress: number };
}

const SEARCH_JOB_FIELDS = `
	id
	query
	state
	createdAt
	startedAt
	finishedAt
	URL
	logURL
	creator { username }
	repoStats { total completed failed inProgress }`;

export const SEARCH_JOBS = `query SearchJobs($first: Int, $after: String, $query: String, $states: [SearchJobState!], $orderBy: SearchJobsOrderBy, $descending: Boolean) {
	searchJobs(first: $first, after: $after, query: $query, states: $states, orderBy: $orderBy, descending: $descending) {
		nodes {${SEARCH_JOB_FIELDS}
		}
		totalCount
		pageInfo { endCursor hasNextPage }
	}
}`;

export type SearchJobsResult = SearchJobsQuery;

export interface SearchJobsVariables {
	first: number;
	after: string | null;
	query: string | null;
	states: SearchJobState[] | null;
	orderBy: 'QUERY' | 'CREATED_AT' | 'STATE';
	descending: boolean;
}

export const VALIDATE_SEARCH_JOB = `query ValidateSearchJob($query: String!) {
	validateSearchJob(query: $query) { alwaysNil }
}`;

export const CREATE_SEARCH_JOB = `mutation CreateSearchJob($query: String!) {
	createSearchJob(query: $query) {${SEARCH_JOB_FIELDS}
	}
}`;

export const CANCEL_SEARCH_JOB = `mutation CancelSearchJob($id: ID!) {
	cancelSearchJob(id: $id) { alwaysNil }
}`;

export const DELETE_SEARCH_JOB = `mutation DeleteSearchJob($id: ID!) {
	deleteSearchJob(id: $id) { alwaysNil }
}`;

/** Whether the job still runs or waits. */
export function isActive(state: SearchJobState): boolean {
	return state === 'QUEUED' || state === 'PROCESSING' || state === 'ERRORED';
}

/** Percentage of repository revisions done (completed or failed). */
export function progressPercent(stats: SearchJobNode['repoStats']): number {
	if (stats.total === 0) return 0;
	return Math.floor(((stats.completed + stats.failed) * 100) / stats.total);
}

/** The CSV download of a job, next to its JSONL `URL`. */
export function csvURL(url: string): string {
	return url.replace(/\.jsonl$/, '.csv');
}
