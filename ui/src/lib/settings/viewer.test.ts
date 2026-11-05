import { describe, expect, it } from 'vitest';
import { safeHref } from '#lib/batches/markdown.ts';
import { defaultPatternType, mergeSettings, notices, quicklinks } from './viewer.ts';

const final = (value: unknown) => JSON.stringify(value);

describe('defaultPatternType', () => {
	it('reads the setting, and treats anything unknown as keyword', () => {
		expect(defaultPatternType(final({ 'search.defaultPatternType': 'regexp' }))).toBe('regexp');
		expect(defaultPatternType(final({ 'search.defaultPatternType': 'fuzzy' }))).toBe('keyword');
		expect(defaultPatternType(final({ 'search.defaultPatternType': 3 }))).toBe('keyword');
		expect(defaultPatternType('{}')).toBe('keyword');
		expect(defaultPatternType('not json')).toBe('keyword');
	});
});

describe('quicklinks', () => {
	it('keeps safe links and drops the rest', () => {
		const links = quicklinks(
			final({
				quicklinks: [
					{ name: 'Docs', url: '/help', description: 'Help' },
					{ name: 'Web', url: 'https://example.com' },
					{ name: 'Script', url: 'javascript:alert(1)' },
					{ name: 'Hidden script', url: 'java\tscript:alert(1)' },
					{ name: '', url: '/empty' },
					'not an object'
				]
			})
		);
		expect(links).toEqual([
			{ name: 'Docs', url: '/help', description: 'Help' },
			{ name: 'Web', url: 'https://example.com', description: null }
		]);
		expect(quicklinks(final({ quicklinks: 'x' }))).toEqual([]);
	});
});

describe('safeHref', () => {
	it('sees through the characters browsers ignore in a URL', () => {
		expect(safeHref('java\tscript:alert(1)')).toBeNull();
		expect(safeHref('\u0001 javascript:alert(1)')).toBeNull();
		expect(safeHref('java\nscript:alert(1)')).toBeNull();
		expect(safeHref(' /help ')).toBe('/help');
	});
});

describe('notices', () => {
	it('filters by location and drops empty messages', () => {
		const doc = final({
			notices: [
				{ message: 'a', location: 'top', dismissible: true },
				{ message: 'b', location: 'home' },
				{ message: ' ', location: 'top' }
			]
		});
		expect(notices(doc, 'top')).toEqual([{ message: 'a', location: 'top', dismissible: true }]);
		expect(notices(doc, 'home')).toEqual([{ message: 'b', location: 'home', dismissible: false }]);
	});
});

describe('mergeSettings', () => {
	it('merges objects key by key and replaces anything else, as the server does', () => {
		expect(
			mergeSettings([{ a: { x: 1, y: [1, 2] }, b: 1, keep: true }, null, { a: { y: [3], z: 1 }, b: { n: 1 } }])
		).toEqual({
			a: { x: 1, y: [3], z: 1 },
			b: { n: 1 },
			keep: true
		});
		expect(mergeSettings([])).toEqual({});
	});
});
