/**
 * GraphQL operations and helpers of the site admin background jobs and log pages
 *: background jobs (leader-elected routines and job
 * queues), the outbound request log, the slow request log and incoming webhook logs.
 */
import type {
	BackgroundJobsQuery,
	OutboundRequestsQuery,
	SlowRequestsQuery,
	WebhookLogsQuery
} from '#lib/graphql-types.ts';

export const BACKGROUND_JOBS = `query BackgroundJobs($recentRunCount: Int) {
	backgroundJobs(recentRunCount: $recentRunCount) {
		nodes {
			id
			name
			routines {
				name
				type
				description
				intervalMs
				instances { hostName lastStartedAt lastStoppedAt }
				recentRuns { at hostName durationMs errorMessage }
				stats { since runCount errorCount minDurationMs avgDurationMs maxDurationMs }
				queue {
					table
					queued
					processing
					completed
					errored
					failed
					canceled
					oldestQueuedAt
					recentFailures { id state failureMessage queuedAt startedAt finishedAt numFailures numResets workerHostname }
					activeJobs { id state failureMessage queuedAt startedAt finishedAt numFailures numResets workerHostname }
				}
			}
		}
	}
}`;

export const RETRY_QUEUE_JOB = `mutation RetryBackgroundQueueJob($queue: String!, $id: ID!) {
	retryBackgroundQueueJob(queue: $queue, id: $id) { alwaysNil }
}`;

export const CANCEL_QUEUE_JOB = `mutation CancelBackgroundQueueJob($queue: String!, $id: ID!) {
	cancelBackgroundQueueJob(queue: $queue, id: $id) { alwaysNil }
}`;

export const OUTBOUND_REQUESTS = `query OutboundRequests($first: Int, $after: String) {
	outboundRequests(first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id
			startedAt
			method
			url
			requestHeaders { name values }
			requestBody
			statusCode
			responseHeaders { name values }
			durationMs
			errorMessage
			creationStackFrame
			callStack
		}
	}
}`;

export const SLOW_REQUESTS = `query SlowRequests($first: Int, $after: String, $kind: SlowRequestKind) {
	slowRequests(first: $first, after: $after, kind: $kind) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			index
			start
			duration
			user { username }
			name
			source
			variables
			errors
			query
			kind
		}
	}
}`;

export const WEBHOOK_LOGS = `query WebhookLogs($first: Int, $after: String, $onlyErrors: Boolean, $onlyUnmatched: Boolean, $webhookID: ID) {
	webhookLogs(first: $first, after: $after, onlyErrors: $onlyErrors, onlyUnmatched: $onlyUnmatched, webhookID: $webhookID) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id
			receivedAt
			statusCode
			event
			externalService { id displayName }
			request { method url headers { name values } body }
			response { body }
		}
	}
}`;

export type RoutineType = 'PERIODIC' | 'PERIODIC_WITH_METRICS' | 'DB_BACKED' | 'CUSTOM';

export interface RoutineInstance {
	hostName: string;
	lastStartedAt: string | null;
	lastStoppedAt: string | null;
}

export interface RoutineRun {
	at: string;
	hostName: string;
	durationMs: number;
	errorMessage: string | null;
}

export interface RoutineStats {
	since: string | null;
	runCount: number;
	errorCount: number;
	minDurationMs: number;
	avgDurationMs: number;
	maxDurationMs: number;
}

export interface QueueJob {
	id: string;
	state: string;
	failureMessage: string | null;
	queuedAt: string;
	startedAt: string | null;
	finishedAt: string | null;
	numFailures: number;
	numResets: number;
	workerHostname: string;
}

export interface Queue {
	table: string;
	queued: number;
	processing: number;
	completed: number;
	errored: number;
	failed: number;
	canceled: number;
	oldestQueuedAt: string | null;
	recentFailures: QueueJob[];
	activeJobs: QueueJob[];
}

export interface Routine {
	name: string;
	type: RoutineType;
	description: string;
	intervalMs: number | null;
	instances: RoutineInstance[];
	recentRuns: RoutineRun[];
	stats: RoutineStats;
	queue: Queue | null;
}

export interface BackgroundJob {
	id: string;
	name: string;
	routines: Routine[];
}

export type BackgroundJobsResult = BackgroundJobsQuery;

export interface HTTPHeader {
	name: string;
	values: string[];
}

export interface PageInfo {
	hasNextPage: boolean;
	endCursor: string | null;
}

export interface OutboundRequest {
	id: string;
	startedAt: string;
	method: string;
	url: string;
	requestHeaders: HTTPHeader[];
	requestBody: string;
	statusCode: number;
	responseHeaders: HTTPHeader[];
	durationMs: number;
	errorMessage: string;
	creationStackFrame: string;
	callStack: string;
}

export type OutboundRequestsResult = OutboundRequestsQuery;

export type SlowRequestKind = 'GRAPHQL' | 'SEARCH';

export interface SlowRequest {
	index: string;
	start: string;
	duration: number;
	user: { username: string } | null;
	name: string;
	source: string;
	variables: string;
	errors: string[];
	query: string;
	kind: SlowRequestKind;
}

export type SlowRequestsResult = SlowRequestsQuery;

export interface WebhookLog {
	id: string;
	receivedAt: string;
	statusCode: number;
	event: string | null;
	externalService: { id: string; displayName: string } | null;
	request: { method: string; url: string; headers: HTTPHeader[]; body: string };
	response: { body: string };
}

export type WebhookLogsResult = WebhookLogsQuery;

/** A duration in milliseconds in words: `850 ms`, `2.5 s`, `3 min 20 s`, `2 h 5 min`. */
export function formatMs(ms: number): string {
	if (ms < 1000) return `${Math.round(ms)} ms`;
	const s = ms / 1000;
	if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
	const min = Math.floor(s / 60);
	if (min < 60) {
		const rest = Math.round(s - min * 60);
		return rest ? `${min} min ${rest} s` : `${min} min`;
	}
	const h = Math.floor(min / 60);
	const restMin = min - h * 60;
	return restMin ? `${h} h ${restMin} min` : `${h} h`;
}

/** Whether the instance holds the routine now (it started after it last stopped). */
export function isRunning(i: RoutineInstance): boolean {
	if (!i.lastStartedAt) return false;
	return !i.lastStoppedAt || Date.parse(i.lastStartedAt) > Date.parse(i.lastStoppedAt);
}

/** The routine's health at a glance. */
export function routineHealth(r: Routine): 'ok' | 'failing' | 'stopped' | 'idle' {
	if (r.queue) {
		return r.queue.failed + r.queue.errored > 0 ? 'failing' : 'ok';
	}
	if (r.recentRuns[0]?.errorMessage) return 'failing';
	if (r.instances.length === 0 && r.recentRuns.length === 0) return 'idle';
	if (r.instances.length > 0 && !r.instances.some(isRunning)) return 'stopped';
	return 'ok';
}

/** The class of an HTTP status: `ok` (2xx/3xx), `bad` (4xx/5xx or no response). */
export function statusClass(status: number): 'ok' | 'bad' {
	return status >= 200 && status < 400 ? 'ok' : 'bad';
}

/** Pretty-prints a JSON body; other text is returned as is. */
export function prettyBody(body: string): string {
	const t = body.trim();
	if (!t.startsWith('{') && !t.startsWith('[')) return body;
	try {
		return JSON.stringify(JSON.parse(t), null, 2);
	} catch {
		return body;
	}
}
