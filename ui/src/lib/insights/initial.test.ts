import { describe, expect, it } from 'vitest';
import { defaultComputeFields, defaultLangStatsFields, defaultSearchFields, newSeries } from './creation.ts';
import {
	DRAFT_KEYS,
	captureGroupFieldsFromURL,
	clearDraft,
	createInsightFromSearchURL,
	initialFields,
	insightDataFromQuery,
	loadDraft,
	sanitizeDraft,
	saveDraft,
	searchFieldsFromURL
} from './initial.ts';

function memoryStorage(): Storage & { data: Map<string, string> } {
	const data = new Map<string, string>();
	return {
		data,
		get length() {
			return data.size;
		},
		clear: () => data.clear(),
		key: (i) => [...data.keys()][i] ?? null,
		getItem: (k) => data.get(k) ?? null,
		setItem: (k, v) => void data.set(k, v),
		removeItem: (k) => void data.delete(k)
	};
}

const throwing: Storage = {
	length: 0,
	clear: () => {},
	key: () => null,
	getItem: () => {
		throw new Error('SecurityError');
	},
	setItem: () => {
		throw new Error('QuotaExceededError');
	},
	removeItem: () => {
		throw new Error('SecurityError');
	}
};

const plain = (series: { name: string; query: string; stroke: string }[]) =>
	series.map(({ name, query, stroke }) => ({ name, query, stroke }));

describe('drafts', () => {
	it('saves, restores and clears a search draft under its key', () => {
		const storage = memoryStorage();
		const f = {
			...defaultSearchFields(),
			title: 'TODOs',
			repoMode: 'urls-list' as const,
			repositories: ['gitlab.com/a/b'],
			series: [newSeries(0, { name: 'TODO', query: 'TODO', seriesId: 'server-id' })],
			stepUnit: 'WEEK' as const,
			stepValue: '3'
		};
		saveDraft('search', f, storage);
		const stored = JSON.parse(storage.data.get('insights.search-insight-creation-ui-v2') ?? 'null');
		expect(stored.series).toEqual(plain(f.series));
		const back = loadDraft('search', storage);
		expect(back).not.toBeNull();
		expect({ ...back, series: plain(back?.series ?? []) }).toEqual({ ...f, series: plain(f.series) });
		// Restored series get fresh form keys and no server id.
		expect(back?.series[0]?.key).not.toBe(f.series[0]?.key);
		expect(back?.series[0]?.seriesId).toBeNull();
		clearDraft('search', storage);
		expect(loadDraft('search', storage)).toBeNull();
	});

	it('keeps one draft per wizard', () => {
		const storage = memoryStorage();
		saveDraft('language-stats', { ...defaultLangStatsFields(), repository: 'gitlab.com/a/b' }, storage);
		saveDraft('compute', { ...defaultComputeFields(), groupBy: 'AUTHOR' }, storage);
		expect([...storage.data.keys()].sort()).toEqual([DRAFT_KEYS.compute, DRAFT_KEYS['language-stats']].sort());
		expect(loadDraft('language-stats', storage)?.repository).toBe('gitlab.com/a/b');
		expect(loadDraft('compute', storage)?.groupBy).toBe('AUTHOR');
		expect(loadDraft('capture-group', storage)).toBeNull();
	});

	it('ignores unreadable drafts and unavailable storage', () => {
		const storage = memoryStorage();
		storage.setItem(DRAFT_KEYS.search, '{not json');
		expect(loadDraft('search', storage)).toBeNull();
		expect(loadDraft('search', throwing)).toBeNull();
		expect(() => saveDraft('search', defaultSearchFields(), throwing)).not.toThrow();
		expect(() => clearDraft('search', throwing)).not.toThrow();
		expect(loadDraft('search', null)).toBeNull();
	});

	it('falls back to defaults for malformed fields', () => {
		const f = sanitizeDraft('capture-group', {
			title: 42,
			repoMode: 'nope',
			repositories: ['a', 3],
			query: 'TODO (\\w+)',
			stepUnit: 'DECADE'
		});
		expect(f).toEqual({
			title: '',
			repoMode: 'search-query',
			repoQuery: '',
			repositories: ['a'],
			query: 'TODO (\\w+)',
			stepUnit: 'MONTH',
			stepValue: '2'
		});
		expect(sanitizeDraft('search', 'x')).toBeNull();
		expect(sanitizeDraft('compute', { series: [] })?.series).toHaveLength(1);
	});
});

describe('URL parameters', () => {
	it('splits a search query into the repository query and the series query', () => {
		expect(insightDataFromQuery('context:global repo:^gitlab\\.com/a$ -repo:b TODO lang:go patterntype:keyword')).toEqual({
			repoQuery: 'repo:^gitlab\\.com/a$ -repo:b',
			seriesQuery: 'TODO lang:go patterntype:keyword'
		});
		expect(insightDataFromQuery('"repo:x" r:y')).toEqual({ repoQuery: 'r:y', seriesQuery: '"repo:x"' });
		expect(insightDataFromQuery('')).toEqual({ repoQuery: '', seriesQuery: '' });
	});

	it('prefills the search wizard from `query`', () => {
		const f = searchFieldsFromURL(new URLSearchParams({ query: 'repo:a TODO patterntype:regexp' }));
		expect(f?.repoMode).toBe('search-query');
		expect(f?.repoQuery).toBe('repo:a');
		expect(f?.series.map((s) => [s.name, s.query])).toEqual([['Search series #1', 'TODO patterntype:regexp']]);
		// No repo: filter: the wizard asks for a repository list.
		expect(searchFieldsFromURL(new URLSearchParams({ query: 'TODO' }))?.repoMode).toBe('urls-list');
	});

	it('prefills the search wizard from encoded trend values', () => {
		// Title, repoQuery and series are double-encoded.
		const series = [{ name: 'Go', query: 'lang:go', stroke: '#123456' }, { name: 'Rust', query: 'lang:rust' }];
		const p = new URLSearchParams();
		p.set('title', encodeURIComponent('Languages & more'));
		p.set('repositories', 'gitlab.com/a,gitlab.com/b');
		p.set('series', encodeURIComponent(JSON.stringify(series)));
		const f = searchFieldsFromURL(p);
		expect(f?.title).toBe('Languages & more');
		expect(f?.repoMode).toBe('urls-list');
		expect(f?.repositories).toEqual(['gitlab.com/a', 'gitlab.com/b']);
		expect(f?.series.map((s) => [s.name, s.query])).toEqual([
			['Go', 'lang:go'],
			['Rust', 'lang:rust']
		]);
		expect(f?.series[0]?.stroke).toBe('#123456');
		expect(searchFieldsFromURL(new URLSearchParams({ repoQuery: 'repo:x' }))?.repoMode).toBe('search-query');
		expect(searchFieldsFromURL(new URLSearchParams({ dashboardId: 'd' }))).toBeNull();
		expect(searchFieldsFromURL(new URLSearchParams({ series: '{bad' }))).toBeNull();
	});

	it('prefills the capture group wizard', () => {
		const p = new URLSearchParams({ groupSearchQuery: encodeURIComponent('go\\s(\\d+)'), repoQuery: 'repo:a' });
		const f = captureGroupFieldsFromURL(p);
		expect(f?.query).toBe('go\\s(\\d+)');
		expect(f?.repoMode).toBe('search-query');
		expect(f?.repoQuery).toBe('repo:a');
		expect(captureGroupFieldsFromURL(new URLSearchParams())).toBeNull();
	});

	it('takes URL values over the draft, and the draft over defaults', () => {
		const storage = memoryStorage();
		saveDraft('search', { ...defaultSearchFields(), title: 'Draft' }, storage);
		expect(initialFields('search', new URLSearchParams({ query: 'TODO' }), storage).title).toBe('');
		expect(initialFields('search', new URLSearchParams({ dashboardId: 'd' }), storage).title).toBe('Draft');
		expect(initialFields('compute', new URLSearchParams({ query: 'TODO' }), storage).series[0]?.query).toBe('');
		expect(initialFields('language-stats', new URLSearchParams(), throwing)).toEqual(defaultLangStatsFields());
	});

	it('links search results to the search wizard', () => {
		const url = createInsightFromSearchURL('repo:a TODO', 'keyword');
		expect(url.startsWith('/trends/create/search?')).toBe(true);
		expect(new URL(url, 'http://x').searchParams.get('query')).toBe('repo:a TODO patterntype:keyword');
		const kept = createInsightFromSearchURL('TODO patternType:regexp', 'keyword');
		expect(new URL(kept, 'http://x').searchParams.get('query')).toBe('TODO patternType:regexp');
	});
});
