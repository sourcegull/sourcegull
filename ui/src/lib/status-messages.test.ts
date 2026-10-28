import { describe, expect, it } from 'vitest';
import {
	cloningSummary,
	indexingSummary,
	percent,
	stateLabel,
	statusEntry,
	summarize,
	syncInProgress,
	type StatusMessage
} from './status-messages.ts';

const cloning: StatusMessage = {
	__typename: 'CloningProgress',
	message: '1,413 repositories enqueued for cloning. 4 repositories currently cloning...',
	cloned: 9,
	cloning: 4,
	queued: 1413,
	total: 1426
};
const paused: StatusMessage = {
	__typename: 'GitserverDiskThresholdReached',
	message: 'Clones are paused on repo node "repo-0": it is low on disk space.',
	address: 'repo-0',
	clonesPaused: true
};

describe('status messages', () => {
	it('summarizes clone and index progress', () => {
		expect(cloningSummary(cloning as Extract<StatusMessage, { __typename: 'CloningProgress' }>)).toBe(
			'Cloning repositories: 9 cloned, 4 cloning, 1,413 queued'
		);
		expect(cloningSummary({ cloned: 3, cloning: 0, queued: 2 })).toBe(
			'Cloning repositories: 3 cloned, 2 queued'
		);
		expect(indexingSummary({ indexed: 12, notIndexed: 28 })).toBe(
			'Indexing repositories: 12 of 40 indexed'
		);
	});

	it('is ok without messages, in progress while cloning, an error when something failed', () => {
		expect(summarize([]).state).toBe('ok');
		expect(summarize([cloning]).state).toBe('progress');
		expect(summarize([{ __typename: 'IndexingProgress', indexed: 1, notIndexed: 2 }]).state).toBe(
			'progress'
		);
		const s = summarize([cloning, paused]);
		expect(s.state).toBe('error');
		// Problems first.
		expect(s.entries.map((e) => e.kind)).toEqual(['error', 'progress']);
		expect(stateLabel('error')).toBe('Repository sync problems');
	});

	it('links each message to where to look', () => {
		expect(statusEntry(paused)).toMatchObject({
			title: 'Clones are paused: low disk space',
			href: '/site-admin/repo-nodes'
		});
		expect(
			statusEntry({
				__typename: 'GitserverDiskThresholdReached',
				message: 'over 90%',
				address: 'repo-0',
				clonesPaused: false
			}).kind
		).toBe('warning');
		expect(
			statusEntry({
				__typename: 'ExternalServiceSyncError',
				message: '401 Unauthorized',
				externalService: { id: 'RXh0ZXJuYWxTZXJ2aWNlOjE=', displayName: 'GitLab' }
			})
		).toMatchObject({
			kind: 'error',
			title: 'Syncing GitLab failed',
			text: '401 Unauthorized',
			href: '/site-admin/external-services/RXh0ZXJuYWxTZXJ2aWNlOjE%3D'
		});
		expect(statusEntry({ __typename: 'SyncError', message: '2 failed' }).href).toBe(
			'/site-admin/repositories?filter=failed'
		);
		expect(statusEntry({ __typename: 'GitUpdatesDisabled', message: 'off' }).kind).toBe('warning');
		expect(statusEntry({ __typename: 'NoRepositoriesDetected', message: 'none' }).kind).toBe('info');
		expect(
			statusEntry({
				__typename: 'DeclaredConfigError',
				message: 'connections[gitlab-main]: unknown key "tokn"',
				source: 'resources',
				path: '/etc/sourcegull/resources.yaml'
			})
		).toMatchObject({
			kind: 'error',
			title: 'Applying SOURCEGULL_RESOURCES_FILE failed',
			text: '/etc/sourcegull/resources.yaml: connections[gitlab-main]: unknown key "tokn"',
			href: '/site-admin/configuration'
		});
		expect(
			statusEntry({
				__typename: 'DeclaredConfigError',
				message: 'syntax error',
				source: 'global_settings',
				path: '/etc/sourcegull/settings.json'
			}).title
		).toBe('Applying SOURCEGULL_GLOBAL_SETTINGS_FILE failed');
	});

	it('refreshes the repositories page only while something is in flight', () => {
		expect(syncInProgress({ cloning: 0, queued: 0, notIndexed: 0 })).toBe(false);
		expect(syncInProgress({ cloning: 1, queued: 0, notIndexed: 0 })).toBe(true);
		expect(syncInProgress({ cloning: 0, queued: 5, notIndexed: 0 })).toBe(true);
		expect(syncInProgress({ cloning: 0, queued: 0, notIndexed: 3 })).toBe(true);
	});

	it('computes progress percentages', () => {
		expect(percent(9, 1426)).toBe(0);
		expect(percent(713, 1426)).toBe(50);
		expect(percent(1425, 1426)).toBe(99);
		expect(percent(1426, 1426)).toBe(100);
		expect(percent(0, 0)).toBe(100);
	});
});
