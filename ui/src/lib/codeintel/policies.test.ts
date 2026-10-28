import { describe, expect, it } from 'vitest';
import { describeRetention, describeScope, describeTarget, formatHours, parsePatterns, splitHours } from './policies.ts';

describe('policies', () => {
	it('formats durations in the largest whole unit', () => {
		expect(formatHours(null)).toBe('forever');
		expect(formatHours(2016)).toBe('12 weeks');
		expect(formatHours(168)).toBe('1 week');
		expect(formatHours(48)).toBe('2 days');
		expect(formatHours(5)).toBe('5 hours');
		expect(formatHours(1)).toBe('1 hour');
	});

	it('splits durations for the editor', () => {
		expect(splitHours(null)).toEqual({ amount: null, unit: 24 });
		expect(splitHours(8064)).toEqual({ amount: 48, unit: 168 });
		expect(splitHours(72)).toEqual({ amount: 3, unit: 24 });
		expect(splitHours(7)).toEqual({ amount: 7, unit: 1 });
	});

	it('describes targets, scopes and retention', () => {
		expect(describeTarget('GIT_TREE', '*')).toBe('All branches');
		expect(describeTarget('GIT_TAG', 'v*')).toBe('Tags matching v*');
		expect(describeTarget('GIT_COMMIT', 'a'.repeat(40))).toBe('Commit aaaaaaa');
		expect(describeScope({ repository: null, repositoryPatterns: null })).toBe('All repositories');
		expect(describeScope({ repository: null, repositoryPatterns: ['a/*', 'b/*'] })).toBe('a/*, b/*');
		expect(describeScope({ repository: { name: 'x/y', url: '/x/y' }, repositoryPatterns: null })).toBe('x/y');
		const base = { type: 'GIT_TREE' as const, retentionEnabled: true, retentionDurationHours: 168, retainIntermediateCommits: true };
		expect(describeRetention(base)).toBe('Kept 1 week, every commit on the branch');
		expect(describeRetention({ ...base, type: 'GIT_TAG' })).toBe('Kept 1 week');
		expect(describeRetention({ ...base, retentionEnabled: false })).toBe('');
	});

	it('parses repository patterns', () => {
		expect(parsePatterns('  \n ')).toBeNull();
		expect(parsePatterns('a/*\n b/* , c')).toEqual(['a/*', 'b/*', 'c']);
	});
});
