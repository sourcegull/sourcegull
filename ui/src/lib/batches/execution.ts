/**
 * The execution views of server-side bulk change specs: overall state and
 * progress, the workspaces with their state, and a workspace's stages and steps. GraphQL
 * operations and pure helpers; the pages poll while the spec is processing.
 */
import type { BatchSpecState } from './spec.ts';
import type { BatchSpecExecutionQuery, BatchSpecWorkspaceQuery } from '#lib/graphql-types.ts';

export type WorkspaceState =
	| 'SKIPPED'
	| 'PENDING'
	| 'QUEUED'
	| 'PROCESSING'
	| 'FAILED'
	| 'COMPLETED'
	| 'CANCELING'
	| 'CANCELED';

const WORKSPACE_LIST_FIELDS = `fragment ExecutionWorkspaceFields on BatchSpecWorkspace {
	__typename
	id
	state
	ignored
	unsupported
	cachedResultFound
	stepCacheResultCount
	queuedAt
	startedAt
	finishedAt
	placeInQueue
	placeInGlobalQueue
	diffStat { added deleted }
	... on VisibleBatchSpecWorkspace {
		repository { name url }
		branch { displayName }
		path
	}
}`;

/** A spec's execution with one page of its workspaces. */
export const BATCH_SPEC_EXECUTION = `query BatchSpecExecution($id: ID!, $first: Int, $search: String, $state: BatchSpecWorkspaceState) {
	node(id: $id) {
		__typename
		... on BatchSpec {
			id
			state
			source
			originalInput
			description { name description }
			createdAt
			startedAt
			finishedAt
			applyURL
			failureMessage
			viewerCanAdminister
			viewerCanRetry
			noCache
			creator { id username }
			namespace { id url }
			diffStat { added deleted }
			workspaceResolution {
				state
				failureMessage
				all: workspaces(first: 0) { totalCount stats { errored completed processing queued ignored } }
				skipped: workspaces(first: 0, state: SKIPPED) { totalCount }
				workspaces(first: $first, search: $search, state: $state) {
					totalCount
					pageInfo { hasNextPage endCursor }
					nodes { ...ExecutionWorkspaceFields }
				}
			}
		}
	}
}
${WORKSPACE_LIST_FIELDS}`;

export interface DiffStat {
	added: number;
	deleted: number;
}

export interface ExecutionWorkspace {
	__typename: 'VisibleBatchSpecWorkspace' | 'HiddenBatchSpecWorkspace';
	id: string;
	state: WorkspaceState;
	ignored: boolean;
	unsupported: boolean;
	cachedResultFound: boolean;
	stepCacheResultCount: number;
	queuedAt: string | null;
	startedAt: string | null;
	finishedAt: string | null;
	placeInQueue: number | null;
	placeInGlobalQueue: number | null;
	diffStat: DiffStat | null;
	repository?: { name: string; url: string };
	branch?: { displayName: string };
	path?: string;
}

export interface ExecutionStats {
	errored: number;
	completed: number;
	processing: number;
	queued: number;
	ignored: number;
}

export interface ExecutionSpec {
	__typename: 'BatchSpec';
	id: string;
	state: BatchSpecState;
	source: 'LOCAL' | 'REMOTE';
	originalInput: string;
	description: { name: string; description: string };
	createdAt: string;
	startedAt: string | null;
	finishedAt: string | null;
	applyURL: string | null;
	failureMessage: string | null;
	viewerCanAdminister: boolean;
	viewerCanRetry: boolean;
	noCache: boolean | null;
	creator: { id: string; username: string } | null;
	namespace: { id: string; url: string };
	diffStat: DiffStat | null;
	workspaceResolution: {
		state: string;
		failureMessage: string | null;
		all: { totalCount: number; stats: ExecutionStats };
		skipped: { totalCount: number };
		workspaces: {
			totalCount: number;
			pageInfo: { hasNextPage: boolean; endCursor: string | null };
			nodes: ExecutionWorkspace[];
		};
	} | null;
}

export type BatchSpecExecutionResult = BatchSpecExecutionQuery;

const LOG_ENTRY_FIELDS = `fragment ExecutionLogFields on ExecutionLogEntry {
	key
	command
	startTime
	exitCode
	out
	durationMilliseconds
}`;

/** One workspace's execution in detail. */
export const BATCH_SPEC_WORKSPACE = `query BatchSpecWorkspace($id: ID!) {
	node(id: $id) {
		__typename
		...ExecutionWorkspaceFields
		... on VisibleBatchSpecWorkspace {
			onlyFetchWorkspace
			searchResultPaths
			failureMessage
			executor { hostname }
			stages {
				setup { ...ExecutionLogFields }
				srcExec { ...ExecutionLogFields }
				teardown { ...ExecutionLogFields }
			}
			steps {
				number
				run
				container
				ifCondition
				cachedResultFound
				skipped
				startedAt
				finishedAt
				exitCode
				environment { name value }
				outputVariables { name value }
				diffStat { added deleted }
				outputLines(first: 500) { totalCount nodes }
			}
			changesetSpecs {
				id
				description {
					__typename
					... on GitBranchChangesetDescription { title headRef diffStat { added deleted } }
				}
			}
		}
	}
}
${WORKSPACE_LIST_FIELDS}
${LOG_ENTRY_FIELDS}`;

export interface LogEntry {
	key: string;
	command: string[];
	startTime: string | null;
	exitCode: number | null;
	out: string;
	durationMilliseconds: number | null;
}

export interface WorkspaceStep {
	number: number;
	run: string;
	container: string;
	ifCondition: string | null;
	cachedResultFound: boolean;
	skipped: boolean;
	startedAt: string | null;
	finishedAt: string | null;
	exitCode: number | null;
	environment: { name: string; value: string | null }[];
	outputVariables: { name: string; value: unknown }[] | null;
	diffStat: DiffStat | null;
	outputLines: { totalCount: number; nodes: string[] };
}

export interface WorkspaceDetail extends ExecutionWorkspace {
	onlyFetchWorkspace?: boolean;
	searchResultPaths?: string[];
	failureMessage?: string | null;
	executor?: { hostname: string } | null;
	stages?: { setup: LogEntry[]; srcExec: LogEntry[]; teardown: LogEntry[] } | null;
	steps?: WorkspaceStep[];
	changesetSpecs?: {
		id: string;
		description: { __typename: string; title?: string; headRef?: string; diffStat?: DiffStat };
	}[] | null;
}

export type BatchSpecWorkspaceResult = BatchSpecWorkspaceQuery;

export const CANCEL_BATCH_SPEC_EXECUTION = `mutation CancelBatchSpecExecution($batchSpec: ID!) {
	cancelBatchSpecExecution(batchSpec: $batchSpec) { id state }
}`;

export const RETRY_BATCH_SPEC_EXECUTION = `mutation RetryBatchSpecExecution($batchSpec: ID!, $includeCompleted: Boolean) {
	retryBatchSpecExecution(batchSpec: $batchSpec, includeCompleted: $includeCompleted) { id state }
}`;

export const RETRY_WORKSPACE_EXECUTION = `mutation RetryBatchSpecWorkspaceExecution($workspaces: [ID!]!) {
	retryBatchSpecWorkspaceExecution(batchSpecWorkspaces: $workspaces) { alwaysNil }
}`;

// ------------------------------------------------------------------ helpers

/** Whether the spec's execution is still going (the page polls meanwhile). */
export function isExecutionActive(state: BatchSpecState): boolean {
	return state === 'QUEUED' || state === 'PROCESSING' || state === 'CANCELING';
}

/** Whether a workspace's execution is still going. */
export function isWorkspaceActive(state: WorkspaceState): boolean {
	return state === 'QUEUED' || state === 'PROCESSING' || state === 'CANCELING';
}

/**
 * Whether the viewer may cancel or retry: the spec's creator or a site admin (as the API
 * checks).
 */
export function canModifyExecution(
	spec: Pick<ExecutionSpec, 'creator'>,
	viewer: { id: string; siteAdmin: boolean } | null
): boolean {
	if (!viewer) return false;
	return viewer.siteAdmin || (spec.creator !== null && spec.creator.id === viewer.id);
}

export function canCancel(spec: Pick<ExecutionSpec, 'state' | 'creator'>, viewer: { id: string; siteAdmin: boolean } | null): boolean {
	return (spec.state === 'QUEUED' || spec.state === 'PROCESSING') && canModifyExecution(spec, viewer);
}

/** A workspace's finished execution can run again (`retryBatchSpecWorkspaceExecution`). */
export function canRetryWorkspace(ws: Pick<ExecutionWorkspace, 'state' | '__typename'>): boolean {
	return (
		ws.__typename === 'VisibleBatchSpecWorkspace' &&
		(ws.state === 'FAILED' || ws.state === 'CANCELED' || ws.state === 'COMPLETED')
	);
}

/** Progress of an execution: finished workspaces among those that run, from 0 to 1. */
export function executionProgress(
	stats: Pick<ExecutionStats, 'completed' | 'errored' | 'processing' | 'queued'>,
	total: number,
	skipped: number
): { done: number; total: number; ratio: number } {
	const runs = Math.max(total - skipped, 0);
	const done = Math.min(stats.completed + stats.errored, runs);
	return { done, total: runs, ratio: runs === 0 ? 1 : done / runs };
}

/** Badge tone of a spec or workspace state (a CSS class suffix). */
export function stateTone(state: string): 'success' | 'danger' | 'info' | 'warning' | 'muted' {
	switch (state) {
		case 'COMPLETED':
			return 'success';
		case 'FAILED':
			return 'danger';
		case 'QUEUED':
		case 'PROCESSING':
			return 'info';
		case 'CANCELING':
		case 'CANCELED':
			return 'warning';
		default:
			return 'muted';
	}
}

/** Human label of a spec or workspace state. */
export function stateLabel(state: string): string {
	const lower = state.toLowerCase();
	return lower.charAt(0).toUpperCase() + lower.slice(1);
}

/** `850 ms`, `12.3 s`, `4m 05s`, `2h 03m`. */
export function formatDuration(ms: number): string {
	if (ms < 1000) return `${Math.max(0, Math.round(ms))} ms`;
	const secs = ms / 1000;
	if (secs < 60) return `${secs.toFixed(1)} s`;
	const whole = Math.round(secs);
	if (whole < 3600) return `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, '0')}s`;
	return `${Math.floor(whole / 3600)}h ${String(Math.floor((whole % 3600) / 60)).padStart(2, '0')}m`;
}

/** The time between two timestamps, up to `now` while unfinished; null before it started. */
export function elapsed(startedAt: string | null, finishedAt: string | null, now: number = Date.now()): number | null {
	if (!startedAt) return null;
	const end = finishedAt ? new Date(finishedAt).getTime() : now;
	return Math.max(0, end - new Date(startedAt).getTime());
}

/** One line of a step's output, split from its `stdout: `/`stderr: ` prefix. */
export interface OutputLine {
	stream: 'stdout' | 'stderr' | null;
	text: string;
}

export function parseOutputLine(line: string): OutputLine {
	const m = /^(stdout|stderr): ?(.*)$/s.exec(line);
	if (m) return { stream: m[1] as 'stdout' | 'stderr', text: m[2] ?? '' };
	return { stream: null, text: line };
}

/** What a step's status badge says. */
export function stepStatus(step: Pick<WorkspaceStep, 'skipped' | 'cachedResultFound' | 'exitCode' | 'startedAt' | 'finishedAt'>): {
	label: string;
	tone: 'success' | 'danger' | 'info' | 'warning' | 'muted';
} {
	if (step.skipped) return { label: 'skipped', tone: 'muted' };
	if (step.cachedResultFound) return { label: 'cached', tone: 'success' };
	if (step.exitCode !== null) {
		return step.exitCode === 0 ? { label: 'exit 0', tone: 'success' } : { label: `exit ${step.exitCode}`, tone: 'danger' };
	}
	if (step.startedAt && !step.finishedAt) return { label: 'running', tone: 'info' };
	return { label: 'not run', tone: 'muted' };
}

/** An output variable's value for display: strings as is, everything else as JSON. */
export function formatOutputValue(value: unknown): string {
	if (typeof value === 'string') return value;
	return JSON.stringify(value, null, 2) ?? String(value);
}

/** Where a queued workspace stands, e.g. `#2 in your queue, #5 overall`. */
export function queuePosition(ws: Pick<ExecutionWorkspace, 'state' | 'placeInQueue' | 'placeInGlobalQueue'>): string | null {
	if (ws.state !== 'QUEUED') return null;
	const parts: string[] = [];
	if (ws.placeInQueue !== null) parts.push(`#${ws.placeInQueue} in your queue`);
	if (ws.placeInGlobalQueue !== null) parts.push(`#${ws.placeInGlobalQueue} overall`);
	return parts.length > 0 ? parts.join(', ') : null;
}

/** `repo@branch` and the workspace path, for headings. */
export function workspaceTitle(ws: Pick<ExecutionWorkspace, 'repository' | 'branch' | 'path'>): string {
	if (!ws.repository) return 'Hidden repository';
	const branch = ws.branch ? `@${ws.branch.displayName}` : '';
	return `${ws.repository.name}${branch}${ws.path ? ` · ${ws.path}` : ''}`;
}
