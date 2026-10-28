/**
 * The site admin's status indicator: `statusMessages`, as the nav item shows them. The query is polled; `summarize` turns the messages into
 * the indicator's state and the popover's entries.
 */
import { sourceVariable } from './declared-config.ts';
import type { StatusMessagesQuery } from '#lib/graphql-types.ts';

/** How often the indicator asks again (ms). */
export const STATUS_MESSAGES_POLL_INTERVAL = 10_000;

/** How often the site admin repositories page refreshes while repositories clone or index (ms). */
export const REPOSITORIES_REFRESH_INTERVAL = 5_000;

export const STATUS_MESSAGES = `query StatusMessages {
	statusMessages {
		__typename
		... on GitUpdatesDisabled { message }
		... on NoRepositoriesDetected { message }
		... on CloningProgress { message cloned cloning queued total }
		... on ExternalServiceSyncError { message externalService { id displayName } }
		... on SyncError { message }
		... on IndexingProgress { notIndexed indexed }
		... on GitserverDiskThresholdReached { message address clonesPaused }
		... on DeclaredConfigError { message source path }
	}
}`;

export type StatusMessage =
	| { __typename: 'GitUpdatesDisabled'; message: string }
	| { __typename: 'NoRepositoriesDetected'; message: string }
	| {
			__typename: 'CloningProgress';
			message: string;
			cloned: number;
			cloning: number;
			queued: number;
			total: number;
	  }
	| {
			__typename: 'ExternalServiceSyncError';
			message: string;
			externalService: { id: string; displayName: string };
	  }
	| { __typename: 'SyncError'; message: string }
	| { __typename: 'IndexingProgress'; notIndexed: number; indexed: number }
	| {
			__typename: 'GitserverDiskThresholdReached';
			message: string;
			address: string;
			clonesPaused: boolean;
	  }
	| { __typename: 'DeclaredConfigError'; message: string; source: string; path: string };

export type StatusMessagesResult = StatusMessagesQuery;

/** The indicator's state: errors win over work in progress. */
export type StatusState = 'ok' | 'progress' | 'error';

export interface StatusEntry {
	kind: 'progress' | 'error' | 'warning' | 'info';
	title: string;
	text: string;
	/** Where to look or act. */
	href: string;
	linkText: string;
}

const n = (v: number) => v.toLocaleString('en-US');

/** "Cloning repositories: 9 cloned, 4 cloning, 1,413 queued". */
export function cloningSummary(m: { cloned: number; cloning: number; queued: number }): string {
	const parts = [`${n(m.cloned)} cloned`];
	if (m.cloning) parts.push(`${n(m.cloning)} cloning`);
	if (m.queued) parts.push(`${n(m.queued)} queued`);
	return `Cloning repositories: ${parts.join(', ')}`;
}

/** "Indexing repositories: 12 of 40 indexed". */
export function indexingSummary(m: { indexed: number; notIndexed: number }): string {
	return `Indexing repositories: ${n(m.indexed)} of ${n(m.indexed + m.notIndexed)} indexed`;
}

/** One popover entry per message. */
export function statusEntry(m: StatusMessage): StatusEntry {
	switch (m.__typename) {
		case 'CloningProgress':
			return {
				kind: 'progress',
				title: cloningSummary(m),
				text: m.message,
				href: '/site-admin/repositories',
				linkText: 'View repositories'
			};
		case 'IndexingProgress':
			return {
				kind: 'progress',
				title: indexingSummary(m),
				text: 'Repositories are searchable without an index, but more slowly.',
				href: '/site-admin/repositories',
				linkText: 'View repositories'
			};
		case 'ExternalServiceSyncError':
			return {
				kind: 'error',
				title: `Syncing ${m.externalService.displayName} failed`,
				text: m.message,
				href: `/site-admin/external-services/${encodeURIComponent(m.externalService.id)}`,
				linkText: 'View code host connection'
			};
		case 'SyncError':
			return {
				kind: 'error',
				title: 'Syncing repositories failed',
				text: m.message,
				href: '/site-admin/repositories?filter=failed',
				linkText: 'View failed repositories'
			};
		case 'GitserverDiskThresholdReached':
			return {
				kind: m.clonesPaused ? 'error' : 'warning',
				title: m.clonesPaused ? 'Clones are paused: low disk space' : 'Repo node low on disk space',
				text: m.message,
				href: '/site-admin/repo-nodes',
				linkText: 'View repo nodes'
			};
		case 'DeclaredConfigError':
			return {
				kind: 'error',
				title: `Applying ${sourceVariable(m.source)} failed`,
				text: `${m.path}: ${m.message}`,
				href: '/site-admin/configuration',
				linkText: 'View configuration files'
			};
		case 'GitUpdatesDisabled':
			return {
				kind: 'warning',
				title: 'Code syncing disabled',
				text: m.message,
				href: '/site-admin/configuration',
				linkText: 'View site configuration'
			};
		case 'NoRepositoriesDetected':
			return {
				kind: 'info',
				title: 'No repositories',
				text: m.message,
				href: '/site-admin/external-services',
				linkText: 'Add a code host connection'
			};
	}
}

export interface StatusSummary {
	state: StatusState;
	/** Errors and warnings first, then progress. */
	entries: StatusEntry[];
}

const ORDER: Record<StatusEntry['kind'], number> = { error: 0, warning: 1, progress: 2, info: 3 };

export function summarize(messages: StatusMessage[]): StatusSummary {
	const entries = messages.map(statusEntry).sort((a, b) => ORDER[a.kind] - ORDER[b.kind]);
	const state: StatusState = entries.some((e) => e.kind === 'error' || e.kind === 'warning')
		? 'error'
		: entries.some((e) => e.kind === 'progress')
			? 'progress'
			: 'ok';
	return { state, entries };
}

/** The indicator's accessible label. */
export function stateLabel(state: StatusState): string {
	switch (state) {
		case 'ok':
			return 'Repositories up to date';
		case 'progress':
			return 'Repositories syncing';
		case 'error':
			return 'Repository sync problems';
	}
}

/** Whether the repositories page should keep refreshing for these counts. */
export function syncInProgress(stats: {
	cloning: number;
	queued: number;
	notIndexed: number;
}): boolean {
	return stats.cloning > 0 || stats.queued > 0 || stats.notIndexed > 0;
}

/** `part / whole` in percent, 0–100, rounded down (100 only when complete). */
export function percent(part: number, whole: number): number {
	if (whole <= 0) return 100;
	return Math.min(100, Math.floor((part * 100) / whole));
}
