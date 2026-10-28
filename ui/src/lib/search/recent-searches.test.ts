import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';
import {
	LOCAL_STORAGE_KEY,
	MAX_RECENT_SEARCHES,
	RECENT_SEARCHES_KEY,
	addRecentSearch,
	createRecentSearches,
	localBackend,
	matchRecentSearches,
	parseRecentSearches,
	recallIndex,
	recentQuery,
	relativeTime,
	removeRecentSearch,
	splitContext,
	resultCountLabel,
	stepIndex,
	type RecentSearch,
	type RecentSearchesBackend
} from './recent-searches.ts';

const entry = (query: string, resultCount = 1, timestamp = '2026-10-22T10:00:00Z'): RecentSearch => ({
	query,
	resultCount,
	limitHit: false,
	timestamp
});

class MemoryStorage {
	map = new Map<string, string>();
	getItem(k: string) {
		return this.map.get(k) ?? null;
	}
	setItem(k: string, v: string) {
		this.map.set(k, v);
	}
}

describe('list operations', () => {
	it('adds newest first, dedupes by query and caps', () => {
		let l: RecentSearch[] = [];
		l = addRecentSearch(l, entry('a', 1));
		l = addRecentSearch(l, entry('b', 2));
		l = addRecentSearch(l, entry(' a ', 3));
		expect(l.map((e) => [e.query, e.resultCount])).toEqual([
			['a', 3],
			['b', 2]
		]);
		expect(addRecentSearch(l, entry('  '))).toBe(l);
		for (let i = 0; i < 30; i++) l = addRecentSearch(l, entry(`q${i}`));
		expect(l).toHaveLength(MAX_RECENT_SEARCHES);
		expect(l[0]?.query).toBe('q29');
	});

	it('removes one', () => {
		expect(removeRecentSearch([entry('a'), entry('b')], 'a').map((e) => e.query)).toEqual(['b']);
	});

	it('parses stored values, dropping invalid entries', () => {
		expect(parseRecentSearches(null)).toEqual([]);
		expect(parseRecentSearches({})).toEqual([]);
		expect(
			parseRecentSearches([
				entry('a'),
				{ query: 'a', timestamp: 'x' },
				{ query: '', timestamp: 'x' },
				{ query: 'no time' },
				'junk',
				{ query: 'b', timestamp: 't', resultCount: 'many', limitHit: true }
			])
		).toEqual([entry('a'), { query: 'b', timestamp: 't', resultCount: 0, limitHit: true }]);
	});

	it('matches typed text', () => {
		const l = [entry('repo:acme refund'), entry('lang:go Refund'), entry('foo')];
		expect(matchRecentSearches(l, '').map((e) => e.query)).toHaveLength(3);
		expect(matchRecentSearches(l, 'REFUND').map((e) => e.query)).toEqual(['repo:acme refund', 'lang:go Refund']);
		expect(matchRecentSearches(l, 'foo')).toEqual([]);
		expect(matchRecentSearches(l, '', 2)).toHaveLength(2);
	});

	it('labels counts and times', () => {
		expect(resultCountLabel(entry('a', 1))).toBe('1 result');
		expect(resultCountLabel(entry('a', 0))).toBe('0 results');
		expect(resultCountLabel({ resultCount: 1500, limitHit: true })).toBe('1500+ results');
		const now = new Date('2026-10-22T12:00:00Z');
		expect(relativeTime('2026-10-22T11:59:30Z', now)).toBe('just now');
		expect(relativeTime('2026-10-22T11:59:00Z', now)).toBe('1 minute ago');
		expect(relativeTime('2026-10-22T09:00:00Z', now)).toBe('3 hours ago');
		expect(relativeTime('2026-10-20T12:00:00Z', now)).toBe('2 days ago');
		expect(relativeTime('2026-01-02T12:00:00Z', now)).toBe('2026-01-02');
		expect(relativeTime('nope', now)).toBe('');
	});

	it('keeps context and non-default pattern types in the query', () => {
		expect(recentQuery('context:global foo', 'keyword')).toBe('context:global foo');
		expect(recentQuery('foo', 'regexp')).toBe('foo patternType:regexp');
		expect(recentQuery('foo patternType:literal', 'regexp')).toBe('foo patternType:literal');
	});
});

describe('keyboard navigation', () => {
	it('steps through rows, wrapping', () => {
		expect(stepIndex(-1, 3, 1)).toBe(0);
		expect(stepIndex(-1, 3, -1)).toBe(2);
		expect(stepIndex(2, 3, 1)).toBe(0);
		expect(stepIndex(0, 3, -1)).toBe(2);
		expect(stepIndex(0, 0, 1)).toBe(-1);
	});

	it('recalls older and newer searches', () => {
		expect(recallIndex(-1, 2, true)).toBe(0);
		expect(recallIndex(0, 2, true)).toBe(1);
		expect(recallIndex(1, 2, true)).toBeNull();
		expect(recallIndex(1, 2, false)).toBe(0);
		expect(recallIndex(0, 2, false)).toBe(-1);
		expect(recallIndex(-1, 2, false)).toBeNull();
	});
});

describe('local storage backend', () => {
	it('round-trips and keeps other temporary settings', async () => {
		const s = new MemoryStorage();
		s.setItem(LOCAL_STORAGE_KEY, JSON.stringify({ other: 1 }));
		const b = localBackend(() => s as unknown as Storage);
		expect(await b.load()).toEqual([]);
		await b.save([entry('a')]);
		expect(JSON.parse(s.getItem(LOCAL_STORAGE_KEY) ?? '')).toEqual({
			other: 1,
			[RECENT_SEARCHES_KEY]: [entry('a')]
		});
		expect(await b.load()).toEqual([entry('a')]);
	});

	it('survives missing, corrupt and throwing storage', async () => {
		expect(await localBackend(() => undefined).load()).toEqual([]);
		const s = new MemoryStorage();
		s.setItem(LOCAL_STORAGE_KEY, '{not json');
		expect(await localBackend(() => s as unknown as Storage).load()).toEqual([]);
		const throwing = {
			getItem() {
				throw new Error('SecurityError');
			},
			setItem() {
				throw new Error('QuotaExceededError');
			}
		} as unknown as Storage;
		const b = localBackend(() => throwing);
		expect(await b.load()).toEqual([]);
		await expect(b.save([entry('a')])).resolves.toBeUndefined();
	});
});

describe('store', () => {
	it('loads once, adds, removes and clears through the backend', async () => {
		let loads = 0;
		const saved: RecentSearch[][] = [];
		const backend: RecentSearchesBackend = {
			async load() {
				loads++;
				return [entry('old')];
			},
			async save(l) {
				saved.push(l);
			}
		};
		const store = createRecentSearches(backend);
		await Promise.all([store.load(), store.load()]);
		expect(loads).toBe(1);
		expect(get(store).map((e) => e.query)).toEqual(['old']);
		await store.add(entry('new', 5));
		expect(get(store).map((e) => e.query)).toEqual(['new', 'old']);
		await store.remove('old');
		await store.clear();
		expect(saved.map((l) => l.map((e) => e.query))).toEqual([['new', 'old'], ['new'], []]);
	});

	it('keeps working when the backend fails', async () => {
		const store = createRecentSearches({
			load: () => Promise.reject(new Error('offline')),
			save: () => Promise.reject(new Error('offline'))
		});
		await store.add(entry('a'));
		expect(get(store).map((e) => e.query)).toEqual(['a']);
	});
});

describe('splitContext', () => {
	it('splits a leading context filter off the query', () => {
		expect(splitContext('context:global repo:x foo')).toEqual({ context: 'context:global ', rest: 'repo:x foo' });
		expect(splitContext('repo:x foo')).toEqual({ context: '', rest: 'repo:x foo' });
		expect(splitContext('context:global')).toEqual({ context: '', rest: 'context:global' });
	});
});
