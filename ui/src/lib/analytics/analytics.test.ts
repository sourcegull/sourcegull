import { describe, expect, it } from 'vitest';
import { anonymousCookie } from '#lib/events.ts';
import {
	bars,
	bucketLabel,
	formatCount,
	formatLatency,
	lastActiveFilter,
	preciseShare,
	relativeTime
} from './queries.ts';

describe('analytics helpers', () => {
	it('formats counts and latencies', () => {
		expect(formatCount(0)).toBe('0');
		expect(formatCount(950)).toBe('950');
		expect(formatCount(1200)).toBe('1.2k');
		expect(formatCount(2000)).toBe('2k');
		expect(formatCount(45_600)).toBe('46k');
		expect(formatCount(3_400_000)).toBe('3.4M');
		expect(formatLatency(null)).toBe('–');
		expect(formatLatency(250)).toBe('≤ 250 ms');
		expect(formatLatency(2500)).toBe('≤ 2.5 s');
		expect(formatLatency(30_000)).toBe('≤ 30 s');
	});

	it('labels buckets in UTC', () => {
		expect(bucketLabel('2026-10-19T00:00:00Z', 'DAILY')).toBe('Oct 19');
		expect(bucketLabel('2026-10-19T00:00:00Z', 'WEEKLY')).toBe('Week of Oct 19');
	});

	it('scales bars to the largest value', () => {
		expect(bars([], 100, 50)).toEqual([]);
		const b = bars([0, 5, 10, 5], 100, 50);
		expect(b).toHaveLength(4);
		expect(b[0]?.h).toBe(0);
		expect(b[2]).toEqual({ x: 51, y: 0, w: 23, h: 50 });
		expect(b[1]?.h).toBe(25);
		expect(b[1]?.y).toBe(25);
		expect(bars([0, 0], 10, 10).every((x) => x.h === 0)).toBe(true);
	});

	it('computes the precise share', () => {
		expect(preciseShare(0, 0)).toBeNull();
		expect(preciseShare(1, 3)).toBe(25);
		expect(preciseShare(2, 1)).toBe(67);
	});

	it('builds last-activity filters', () => {
		const now = new Date('2026-10-21T00:00:00Z');
		expect(lastActiveFilter('all', now)).toBeNull();
		expect(lastActiveFilter('active-week', now)).toEqual({ gte: '2026-10-14T00:00:00.000Z' });
		expect(lastActiveFilter('inactive-month', now)).toEqual({ gte: '2026-09-21T00:00:00.000Z', not: true });
		expect(lastActiveFilter('never', now)).toEqual({ empty: true });
	});

	it('describes last activity', () => {
		const now = new Date('2026-10-21T12:00:00Z');
		expect(relativeTime(null, now)).toBe('Never');
		expect(relativeTime('2026-10-21T11:59:30Z', now)).toBe('just now');
		expect(relativeTime('2026-10-21T11:00:00Z', now)).toBe('1 hour ago');
		expect(relativeTime('2026-10-18T12:00:00Z', now)).toBe('3 days ago');
		expect(relativeTime('2025-10-01T12:00:00Z', now)).toBe('1 year ago');
	});

	it('keeps the anonymous id in a cookie the server can read', () => {
		expect(anonymousCookie('3f2a-9c')).toBe(
			'sourcegullAnonymousUid=3f2a-9c; path=/; max-age=31536000; SameSite=Lax'
		);
		expect(anonymousCookie('x', true)).toMatch(/; Secure$/);
	});
});
