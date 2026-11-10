import { describe, expect, it } from 'vitest';
import {
	cursorPages,
	firstPage,
	hasNextPage,
	hasPreviousPage,
	nextPage,
	pageAfter,
	pageIndex,
	pageLoaded,
	previousPage
} from './cursor-pages.ts';

describe('cursor pages', () => {
	it('walks forward and back through the cursors', () => {
		const p = cursorPages(50);
		expect([pageAfter(p), pageIndex(p), hasPreviousPage(p), hasNextPage(p)]).toEqual([null, 0, false, false]);
		pageLoaded(p, { hasNextPage: true, endCursor: 'c1' });
		nextPage(p);
		expect([pageAfter(p), pageIndex(p), hasPreviousPage(p)]).toEqual(['c1', 1, true]);
		pageLoaded(p, { hasNextPage: false, endCursor: null });
		expect(hasNextPage(p)).toBe(false);
		nextPage(p);
		expect(pageIndex(p)).toBe(1);
		previousPage(p);
		expect([pageAfter(p), pageIndex(p)]).toEqual([null, 0]);
	});

	it('starts over on firstPage', () => {
		const p = cursorPages(10);
		pageLoaded(p, { hasNextPage: true, endCursor: 'c1' });
		nextPage(p);
		firstPage(p);
		expect([pageAfter(p), pageIndex(p), hasNextPage(p)]).toEqual([null, 0, false]);
	});
});
