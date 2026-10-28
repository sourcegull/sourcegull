/**
 * The repository's "Code graph data" page: precise indexes uploaded for
 * the repository and auto-indexing jobs, their state, errors and execution logs, and (site
 * admins) delete and reindex.
 */
import type { IndexConfigurationQuery, PreciseIndexesQuery } from '#lib/graphql-types.ts';

export type PreciseIndexState =
	| 'UPLOADING_INDEX'
	| 'QUEUED_FOR_PROCESSING'
	| 'PROCESSING'
	| 'PROCESSING_ERRORED'
	| 'COMPLETED'
	| 'DELETING'
	| 'DELETED'
	| 'QUEUED_FOR_INDEXING'
	| 'INDEXING'
	| 'INDEXING_ERRORED'
	| 'INDEXING_COMPLETED';

export interface PreciseIndexNode {
	id: string;
	inputCommit: string;
	inputRoot: string;
	inputIndexer: string;
	state: PreciseIndexState;
	/** Null until an index job uploads its index. */
	uploadedAt: string | null;
	/** When the auto-indexing job was queued (null for uploads from elsewhere). */
	queuedAt: string | null;
	indexingStartedAt: string | null;
	indexingFinishedAt: string | null;
	shouldReindex: boolean;
	steps: IndexSteps | null;
	processingStartedAt: string | null;
	processingFinishedAt: string | null;
	failure: string | null;
	placeInQueue: number | null;
	uploadSize: number | null;
	numDocuments: number | null;
	numOccurrences: number | null;
	/** The GitLab CI job that uploaded with its job token. */
	gitlabCiJob: { jobId: string; ref: string; username: string } | null;
	/** No retention policy keeps the index any more. */
	expired: boolean;
	indexer: { key: string; name: string; url: string } | null;
	projectRoot: { url: string; commit: { abbreviatedOID: string; url: string } } | null;
}

export interface ExecutionLogEntry {
	key: string;
	command: string[];
	startTime: string | null;
	exitCode: number | null;
	/** Empty for viewers who are not site admins. */
	out: string;
	durationMilliseconds: number | null;
}

/** An auto-indexing job's configuration and execution logs. */
export interface IndexSteps {
	setup: ExecutionLogEntry[];
	preIndex: { root: string; image: string; commands: string[]; logEntry: ExecutionLogEntry | null }[];
	index: {
		commands: string[];
		indexerArgs: string[];
		outfile: string | null;
		requestedEnvVars: string[] | null;
		logEntry: ExecutionLogEntry | null;
	};
	upload: ExecutionLogEntry | null;
	teardown: ExecutionLogEntry[];
}

const LOG_ENTRY = 'key command startTime exitCode out durationMilliseconds';

export const STEPS_FIELDS = `steps {
	setup { ${LOG_ENTRY} }
	preIndex { root image commands logEntry { ${LOG_ENTRY} } }
	index { commands indexerArgs outfile requestedEnvVars logEntry { ${LOG_ENTRY} } }
	upload { ${LOG_ENTRY} }
	teardown { ${LOG_ENTRY} }
}`;

export const PRECISE_INDEXES = `query PreciseIndexes($repo: ID!, $query: String, $states: [PreciseIndexState!],
		$indexerKey: String, $first: Int, $after: String) {
	preciseIndexes(repo: $repo, query: $query, states: $states, indexerKey: $indexerKey,
			first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id inputCommit inputRoot inputIndexer state uploadedAt processingStartedAt
			processingFinishedAt failure placeInQueue uploadSize numDocuments numOccurrences expired
			queuedAt indexingStartedAt indexingFinishedAt shouldReindex
			gitlabCiJob { jobId ref username }
			${STEPS_FIELDS}
			indexer { key name url }
			projectRoot { url commit { abbreviatedOID url } }
		}
	}
	indexerKeys(repo: $repo)
}`;

export type PreciseIndexesResult = PreciseIndexesQuery;

export interface PreciseIndexesVariables {
	repo: string;
	query: string | null;
	states: PreciseIndexState[] | null;
	indexerKey: string | null;
	first: number;
	after: string | null;
}

export const DELETE_PRECISE_INDEX = `mutation DeletePreciseIndex($id: ID!) {
	deletePreciseIndex(id: $id) { alwaysNil }
}`;

export const REINDEX_PRECISE_INDEX = `mutation ReindexPreciseIndex($id: ID!) {
	reindexPreciseIndex(id: $id) { alwaysNil }
}`;

/** State filter choices of the page (label → states). */
export const STATE_FILTERS: { label: string; states: PreciseIndexState[] | null }[] = [
	{ label: 'All', states: null },
	{ label: 'Completed', states: ['COMPLETED'] },
	{ label: 'Errored', states: ['PROCESSING_ERRORED', 'INDEXING_ERRORED'] },
	{ label: 'Queued or processing', states: ['UPLOADING_INDEX', 'QUEUED_FOR_PROCESSING', 'PROCESSING'] },
	{ label: 'Queued or indexing', states: ['QUEUED_FOR_INDEXING', 'INDEXING'] },
	{ label: 'Deleting', states: ['DELETING'] }
];

/** A human label and badge style for a state. */
export function stateLabel(state: PreciseIndexState): { text: string; tone: 'success' | 'danger' | 'info' | 'muted' } {
	switch (state) {
		case 'UPLOADING_INDEX':
			return { text: 'Uploading', tone: 'info' };
		case 'QUEUED_FOR_PROCESSING':
			return { text: 'Queued', tone: 'info' };
		case 'PROCESSING':
			return { text: 'Processing', tone: 'info' };
		case 'PROCESSING_ERRORED':
			return { text: 'Errored', tone: 'danger' };
		case 'COMPLETED':
			return { text: 'Completed', tone: 'success' };
		case 'DELETING':
			return { text: 'Deleting', tone: 'muted' };
		case 'DELETED':
			return { text: 'Deleted', tone: 'muted' };
		case 'QUEUED_FOR_INDEXING':
			return { text: 'Queued for indexing', tone: 'info' };
		case 'INDEXING':
			return { text: 'Indexing', tone: 'info' };
		case 'INDEXING_ERRORED':
			return { text: 'Indexing errored', tone: 'danger' };
		case 'INDEXING_COMPLETED':
			return { text: 'Indexed', tone: 'info' };
	}
}

/** Whether a site admin may reindex an index in `state` (the server enforces it too). */
export function canReindex(state: PreciseIndexState): boolean {
	return (
		state === 'COMPLETED' ||
		state === 'PROCESSING_ERRORED' ||
		state === 'INDEXING_ERRORED' ||
		state === 'INDEXING_COMPLETED'
	);
}

/** Whether an index can still be deleted. */
export function canDelete(state: PreciseIndexState): boolean {
	return state !== 'DELETING' && state !== 'DELETED';
}

/** The directory an index covers, for display (`/` for the repository root). */
export function displayRoot(root: string): string {
	return root === '' ? '/' : root;
}

/** The steps of a job as rows of the log view: label, configuration and log entry. */
export function stepRows(
	steps: IndexSteps
): { label: string; detail: string; entry: ExecutionLogEntry | null }[] {
	const rows: { label: string; detail: string; entry: ExecutionLogEntry | null }[] = [];
	for (const e of steps.setup) rows.push({ label: 'Setup', detail: e.command.join(' '), entry: e });
	steps.preIndex.forEach((s, i) =>
		rows.push({
			label: `Pre-index step ${i + 1}`,
			detail: `${s.image} in ${displayRoot(s.root)}: ${s.commands.join('; ')}`,
			entry: s.logEntry
		})
	);
	rows.push({
		label: 'Indexer',
		detail: [...steps.index.commands, steps.index.indexerArgs.join(' ')].filter((c) => c).join('; '),
		entry: steps.index.logEntry
	});
	if (steps.upload) rows.push({ label: 'Upload', detail: '', entry: steps.upload });
	for (const e of steps.teardown) rows.push({ label: 'Teardown', detail: e.command.join(' '), entry: e });
	return rows;
}

/** The index configuration of a repository. */
export const INDEX_CONFIGURATION = `query IndexConfiguration($repo: ID!) {
	node(id: $repo) {
		... on Repository {
			indexConfiguration {
				configuration
				inferredConfiguration { configuration limitError parsedConfiguration { root indexer { name } } }
			}
		}
	}
}`;

export interface InferredConfiguration {
	configuration: string;
	limitError: string | null;
	parsedConfiguration: { root: string; indexer: { name: string } | null }[] | null;
}

export type IndexConfigurationResult = IndexConfigurationQuery;

export const UPDATE_INDEX_CONFIGURATION = `mutation UpdateRepositoryIndexConfiguration($repo: ID!, $configuration: String!) {
	updateRepositoryIndexConfiguration(repository: $repo, configuration: $configuration) { alwaysNil }
}`;

export const QUEUE_AUTO_INDEX_JOBS = `mutation QueueAutoIndexJobsForRepo($repo: ID!, $rev: String, $configuration: String) {
	queueAutoIndexJobsForRepo(repository: $repo, rev: $rev, configuration: $configuration) { id inputRoot }
}`;
