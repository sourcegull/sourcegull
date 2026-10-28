import { describe, expect, it } from 'vitest';
import type { PreciseIndexCoverageNode } from '#lib/queries.ts';
import { ago, indexedSummary, stateInfo } from './coverage.ts';

const now = new Date('2026-10-20T12:00:00Z');
const node: PreciseIndexCoverageNode = {
	repositoryID: 'UmVwb3NpdG9yeTox',
	repositoryName: 'gitlab.example.com/a/b',
	codeGraphURL: '/gitlab.example.com/a/b/-/code-graph',
	state: 'UP_TO_DATE',
	headCommit: 'a'.repeat(40),
	indexedCommit: 'a'.repeat(40),
	indexedAt: '2026-10-20T09:00:00Z',
	indexedAtHead: true,
	failureMessage: null,
	failedAt: null,
	failureSource: null
};

describe('coverage', () => {
	it('summarizes the indexed commit', () => {
		expect(indexedSummary(node, now)).toBe('aaaaaaaaaaaa · default branch tip · 3h ago');
		expect(
			indexedSummary({ ...node, indexedAtHead: false, indexedAt: '2026-10-08T12:00:00Z' }, now)
		).toBe('aaaaaaaaaaaa · tip is aaaaaaaaaaaa · 12d ago');
		expect(indexedSummary({ ...node, indexedCommit: null }, now)).toBe('never');
	});
	it('labels states', () => {
		expect(stateInfo('FAILING').label).toBe('Failing');
		expect(stateInfo('NOT_TARGETED').badge).toBe('');
		expect(ago('2026-10-20T11:59:30Z', now)).toBe('30s ago');
	});
});
