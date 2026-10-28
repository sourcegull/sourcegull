import { describe, expect, it } from 'vitest';
import type { RankingRunSummary } from '#lib/queries.ts';
import { rankingStatus } from './ranking.ts';

const now = new Date('2026-10-20T12:00:00Z');
const run: RankingRunSummary = {
	graphKey: '4',
	visibleToSearch: true,
	startedAt: '2026-10-20T10:00:00Z',
	completedAt: '2026-10-20T10:00:05Z',
	failureMessage: null
};
const base = {
	derivativeGraphKey: '4',
	enabled: true,
	numExportedIndexes: 3,
	numRankedRepositories: 2,
	numRankedPaths: 1200,
	rankingSummary: [run]
};

describe('rankingStatus', () => {
	it('summarizes the current ranks', () => {
		expect(rankingStatus(base, now)).toEqual({
			text: '1,200 files in 2 repositories ranked from 3 indexes, 2h ago.',
			failed: false
		});
	});

	it('reports off, no runs, running and failed runs', () => {
		expect(rankingStatus({ ...base, enabled: false }, now).text).toMatch(/^off/);
		expect(rankingStatus({ ...base, rankingSummary: [] }, now).text).toMatch(/^no runs yet/);
		const running: RankingRunSummary = { ...run, completedAt: null };
		expect(rankingStatus({ ...base, rankingSummary: [running] }, now).text).toBe('computing since 2h ago…');
		const failed: RankingRunSummary = { ...running, completedAt: '2026-10-20T10:00:01Z', failureMessage: 'upload 3: gone' };
		expect(rankingStatus({ ...base, rankingSummary: [failed] }, now)).toEqual({
			text: 'the last run failed 2h ago: upload 3: gone',
			failed: true
		});
	});
});
