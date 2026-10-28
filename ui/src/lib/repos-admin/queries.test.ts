import { describe, expect, it } from 'vitest';
import {
	cloneState,
	filterVariables,
	formatBytes,
	parseFilter,
	rebalanceSummary,
	relativeTime,
	remoteWebURL,
	type RepoRebalance
} from './queries.ts';

describe('repository admin helpers', () => {
	it('formats byte sizes', () => {
		expect(formatBytes('0')).toBe('0 B');
		expect(formatBytes(1023)).toBe('1023 B');
		expect(formatBytes('1536')).toBe('1.5 KiB');
		expect(formatBytes(5 * 1024 ** 3)).toBe('5.0 GiB');
		expect(formatBytes('123456789012')).toBe('115 GiB');
		expect(formatBytes(null)).toBe('0 B');
	});

	it('formats relative times', () => {
		const now = Date.parse('2026-01-01T12:00:00Z');
		expect(relativeTime(null, now)).toBe('—');
		expect(relativeTime('2026-01-01T11:59:30Z', now)).toBe('30s ago');
		expect(relativeTime('2026-01-01T09:00:00Z', now)).toBe('3h ago');
		expect(relativeTime('2025-12-30T12:00:00Z', now)).toBe('2d ago');
		expect(relativeTime('2026-01-01T12:05:00Z', now)).toBe('in 5m');
	});

	it('maps filters to repositories arguments', () => {
		expect(filterVariables('all')).toEqual({
			cloneStatus: null,
			failedFetch: null,
			corrupted: null,
			indexed: null,
			notIndexed: null
		});
		expect(filterVariables('not-cloned').cloneStatus).toBe('NOT_CLONED');
		expect(filterVariables('failed').failedFetch).toBe(true);
		expect(filterVariables('indexed').notIndexed).toBe(false);
		expect(filterVariables('not-indexed').indexed).toBe(false);
		expect(filterVariables('corrupted').corrupted).toBe(true);
	});

	it('names the clone state', () => {
		const m = {
			cloned: false,
			cloneInProgress: true,
			cloneProgress: null,
			isCorrupted: false,
			updatedAt: null,
			lastChanged: null,
			lastError: null,
			byteSize: '0',
			shard: null,
			isEmpty: false,
			remoteUnavailable: null
		};
		expect(cloneState(m)).toBe('cloning');
		expect(cloneState({ ...m, cloneInProgress: false })).toBe('not cloned');
		expect(cloneState({ ...m, cloneInProgress: false, cloned: true })).toBe('cloned');
	});

	it('summarizes rebalancing', () => {
		const r: RepoRebalance = {
			waitingFor: [],
			queued: 0,
			copying: 0,
			flipped: 0,
			failed: 0,
			done: 4,
			cancelled: 0,
			nodes: [],
			moves: []
		};
		expect(rebalanceSummary(r)).toBe('Balanced: every repository is on its node.');
		expect(rebalanceSummary({ ...r, copying: 2, queued: 15, failed: 1 })).toBe(
			'Moving repositories: 2 copying, 15 queued, 1 failed.'
		);
		expect(rebalanceSummary({ ...r, waitingFor: ['http://repo-3:3178'] })).toContain(
			'Waiting for http://repo-3:3178'
		);
	});

	it('reads the filter from the page URL', () => {
		expect(parseFilter('failed')).toBe('failed');
		expect(parseFilter('not-cloned')).toBe('not-cloned');
		expect(parseFilter(null)).toBe('all');
		expect(parseFilter('bogus')).toBe('all');
	});
});

describe('remoteWebURL', () => {
	it('links clone URLs to the project page', () => {
		expect(remoteWebURL('https://gitlab.example.com/bi/avail_uff.git')).toBe('https://gitlab.example.com/bi/avail_uff');
		expect(remoteWebURL('https://oauth2:secret@gitlab.example.com/a/b.git')).toBe('https://gitlab.example.com/a/b');
		expect(remoteWebURL('git@gitlab.example.com:group/sub/repo.git')).toBe('https://gitlab.example.com/group/sub/repo');
		expect(remoteWebURL('ssh://git@gitlab.example.com:2222/group/repo.git')).toBe('https://gitlab.example.com/group/repo');
		expect(remoteWebURL('/srv/repos/local.git')).toBeNull();
		expect(remoteWebURL('')).toBeNull();
	});
});
