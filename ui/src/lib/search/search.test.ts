import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { formatRepositoryLines, parseRepositoryLines } from './context-form.ts';
import { chunkLines, commitLines, mergeChunks, segments } from './highlight.ts';
import {
	QueryParser,
	appendFilter,
	exactRepo,
	includesArchived,
	removeFilter,
	searchQueryURL,
	searchURL,
	splitContext,
	withArchivedDefault,
	withContext,
	withIncludeArchived,
	withPatternType,
	wordAt
} from './query.ts';
import { includeArchivedSetting } from './queries.ts';
import { SSEParser, dispatch, matchCount, shownBranches, streamSearch, streamURL, type SearchMatch } from './stream.ts';

describe('archived filter', () => {
	it('reads the checkbox state from the query', () => {
		expect(includesArchived('foo')).toBe(false);
		expect(includesArchived('repo:acme foo')).toBe(false);
		expect(includesArchived('foo archived:yes')).toBe(true);
		expect(includesArchived('foo archived:ONLY')).toBe(true);
		expect(includesArchived('foo archived:no')).toBe(false);
		expect(includesArchived('foo-archived:yes')).toBe(false);
	});

	it('has no checkbox when the query names one repository', () => {
		expect(includesArchived(String.raw`repo:^gitlab\.com/a/b$ foo`)).toBeNull();
		expect(includesArchived('repo:^gitlab.com/a/b$@main foo')).toBeNull();
		expect(includesArchived('r:^a/b$')).toBeNull();
		expect(includesArchived('repo:^a/b$ archived:no foo')).toBe(false);
		expect(includesArchived('-repo:^a/b$ foo')).toBe(false);
		expect(includesArchived('repo:^a$|^b$ foo')).toBe(false);
		expect(includesArchived('repo:^a.*$ foo')).toBe(false);
		expect(includesArchived(String.raw`repo:^a\d$ foo`)).toBe(false);
	});

	it('changes the query with the checkbox', () => {
		expect(withIncludeArchived('foo', true, false)).toBe('foo archived:yes');
		expect(withIncludeArchived('foo archived:yes bar', false, false)).toBe('foo bar');
		expect(withIncludeArchived('foo archived:only', true, false)).toBe('foo archived:yes');
		// The setting would add the filter back, so turning it off is written down.
		expect(withIncludeArchived('foo archived:yes', false, true)).toBe('foo archived:no');
		expect(withIncludeArchived('foo archived:no', true, true)).toBe('foo archived:yes');
	});

	it('adds the default only when the query says nothing', () => {
		expect(withArchivedDefault('foo')).toBe('foo archived:yes');
		expect(withArchivedDefault('foo archived:no')).toBe('foo archived:no');
		expect(withArchivedDefault('foo archived:only')).toBe('foo archived:only');
		expect(withArchivedDefault('repo:^a/b$ foo')).toBe('repo:^a/b$ foo');
		expect(withArchivedDefault('  ')).toBe('  ');
	});

	it('reads the setting from merged settings', () => {
		expect(includeArchivedSetting('{"search.includeArchived": true}')).toBe(true);
		expect(includeArchivedSetting('{"search.includeArchived": false}')).toBe(false);
		expect(includeArchivedSetting('{}')).toBe(false);
		expect(includeArchivedSetting('not json')).toBe(false);
	});
});

describe('query helpers', () => {
	it('splits and restores the context', () => {
		expect(splitContext('context:@me/x repo:a foo')).toEqual({ context: '@me/x', rest: 'repo:a foo' });
		expect(splitContext('foo context:global bar')).toEqual({ context: 'global', rest: 'foo bar' });
		expect(splitContext('foo')).toEqual({ context: null, rest: 'foo' });
		expect(splitContext('context:"a b" x')).toEqual({ context: 'a b', rest: 'x' });
		expect(withContext('foo', 'global')).toBe('context:global foo');
		expect(withContext('context:x foo', 'global')).toBe('context:x foo');
		expect(withContext('foo', null)).toBe('foo');
	});

	it('adds and removes filters once', () => {
		expect(appendFilter('foo', 'lang:go')).toBe('foo lang:go');
		expect(appendFilter('foo lang:go', 'lang:go')).toBe('foo lang:go');
		expect(removeFilter('foo lang:go bar', 'lang:go')).toBe('foo bar');
	});

	it('builds URLs and moves patternType: to the parameter', () => {
		expect(searchURL('a b')).toBe('/search?q=a+b');
		expect(searchURL('a', 'regexp')).toBe('/search?q=a&patternType=regexp');
		expect(searchQueryURL('foo patternType:regexp bar')).toBe('/search?q=foo+bar&patternType=regexp');
		expect(withPatternType('foo', 'keyword')).toBe('foo patternType:keyword');
		expect(withPatternType('foo patterntype:literal', 'keyword')).toBe('foo patterntype:literal');
		expect(exactRepo('gitlab.com/a.b')).toBe('^gitlab\\.com/a\\.b$');
	});

	it('finds the word under the cursor', () => {
		expect(wordAt('foo rep', 7)).toEqual({ kind: 'word', from: 4, prefix: 'rep' });
		expect(wordAt('foo -repo:gi', 12)).toEqual({ kind: 'value', field: 'repo', from: 10, prefix: 'gi' });
		expect(wordAt('(lang:', 6)).toEqual({ kind: 'value', field: 'lang', from: 6, prefix: '' });
	});
});

describe('result branches', () => {
	it('labels a result unless it is on the default branch alone', () => {
		expect(shownBranches({})).toEqual([]);
		expect(shownBranches({ branches: ['HEAD'] })).toEqual([]);
		expect(shownBranches({ branches: ['dev'] })).toEqual(['dev']);
		expect(shownBranches({ branches: ['HEAD', 'dev'] })).toEqual(['HEAD', 'dev']);
	});
});

describe('SSE parsing', () => {
	it('handles split chunks, CRLF, multi-line data and comments', () => {
		const p = new SSEParser();
		expect(p.push('event: matches\nda')).toEqual([]);
		expect(p.push('ta: [1]\n\n: ping\n\nevent: done\r\ndata: {}\r\n\r\n')).toEqual([
			{ event: 'matches', data: '[1]' },
			{ event: 'done', data: '{}' }
		]);
		expect(p.push('data: a\ndata: b\n\n')).toEqual([{ event: 'message', data: 'a\nb' }]);
	});

	it('dispatches events', () => {
		const seen: string[] = [];
		const h = {
			onMatches: (m: SearchMatch[]) => seen.push(`m${m.length}`),
			onError: (e: string) => seen.push(`e:${e}`),
			onDone: () => seen.push('done')
		};
		dispatch({ event: 'matches', data: '[]' }, h);
		dispatch({ event: 'error', data: '{"message":"boom"}' }, h);
		dispatch({ event: 'done', data: '{}' }, h);
		expect(seen).toEqual(['m0', 'e:boom', 'done']);
	});

	it('streams a response and reports HTTP errors', async () => {
		const body =
			'event: progress\ndata: {"done":true,"matchCount":2,"durationMs":5,"skipped":[]}\n\nevent: done\ndata: {}\n\n';
		const seen: string[] = [];
		const fakeFetch = (async (url: string) => {
			expect(url).toBe(streamURL({ query: 'x', patternType: 'keyword', display: 10 }));
			return new Response(body, { status: 200 });
		}) as unknown as typeof fetch;
		await streamSearch(
			{ query: 'x', patternType: 'keyword', display: 10 },
			{ onProgress: (p) => seen.push(`p${p.matchCount}`), onDone: () => seen.push('done') },
			undefined,
			fakeFetch
		);
		expect(seen).toEqual(['p2', 'done']);
		const errFetch = (async () => new Response('bad query', { status: 400 })) as unknown as typeof fetch;
		const errors: string[] = [];
		await streamSearch({ query: 'x', patternType: 'keyword' }, { onError: (e) => errors.push(e) }, undefined, errFetch);
		expect(errors).toEqual(['bad query']);
		expect(streamURL({ query: 'a b', patternType: 'regexp' })).toBe('/.api/search/stream?q=a+b&v=V3&t=regexp&cm=t');
	});

	it('counts result units per match', () => {
		const m: SearchMatch = {
			type: 'content',
			path: 'a',
			repositoryID: 1,
			repository: 'r',
			commit: 'c',
			chunkMatches: [
				{
					content: 'x',
					contentStart: { offset: 0, line: 0, column: 0 },
					ranges: [
						{ start: { offset: 0, line: 0, column: 0 }, end: { offset: 1, line: 0, column: 1 } },
						{ start: { offset: 0, line: 0, column: 0 }, end: { offset: 1, line: 0, column: 1 } }
					]
				}
			]
		};
		expect(matchCount(m)).toBe(2);
	});
});

describe('match highlighting', () => {
	const loc = (line: number, column: number) => ({ offset: 0, line, column });

	it('maps byte columns to string offsets', () => {
		// "é" is 2 bytes, "😀" 4 bytes (2 UTF-16 units).
		expect(segments('é😀ab', [[6, 7]])).toEqual([
			{ text: 'é😀', match: false },
			{ text: 'a', match: true },
			{ text: 'b', match: false }
		]);
		expect(segments('abc', [])).toEqual([{ text: 'abc', match: false }]);
		expect(
			segments('abcd', [
				[0, 2],
				[1, 3]
			])
		).toEqual([
			{ text: 'ab', match: true },
			{ text: 'c', match: true },
			{ text: 'd', match: false }
		]);
	});

	it('splits chunk ranges across lines', () => {
		const lines = chunkLines({
			content: 'foo bar\nbaz qux\n',
			contentStart: loc(9, 0),
			ranges: [{ start: loc(9, 4), end: loc(10, 3) }]
		});
		expect(lines.map((l) => l.number)).toEqual([10, 11]);
		expect(lines[0]?.segments).toEqual([
			{ text: 'foo ', match: false },
			{ text: 'bar', match: true }
		]);
		expect(lines[1]?.segments[0]).toEqual({ text: 'baz', match: true });
	});

	it('merges adjacent chunks', () => {
		const c = (line: number, content: string) => ({ content, contentStart: loc(line, 0), ranges: [] });
		const groups = mergeChunks([c(5, 'f\ng'), c(0, 'a\nb'), c(2, 'c')]);
		expect(groups.map((g) => g.map((l) => l.number))).toEqual([
			[1, 2, 3],
			[6, 7]
		]);
	});

	it('strips the code fence of commit matches', () => {
		const { language, lines } = commitLines({
			type: 'commit',
			label: '',
			url: '',
			detail: '',
			repositoryID: 1,
			repository: 'r',
			oid: 'x',
			message: '',
			authorName: '',
			authorDate: '',
			committerName: '',
			committerDate: '',
			content: '```diff\na.rs a.rs\n@@ -1 +1 @@\n-old\n+new\n```',
			ranges: [[4, 1, 3]]
		});
		expect(language).toBe('diff');
		expect(lines.map((l) => l.marker)).toEqual(['a', '@', '-', '+']);
		expect(lines[3]?.segments).toEqual([
			{ text: '+', match: false },
			{ text: 'new', match: true }
		]);
	});
});

describe('context form', () => {
	it('parses and formats repository lines', () => {
		const repos = parseRepositoryLines('a/b\n# comment\n\n c/d@main, v1 \n');
		expect(repos).toEqual([
			{ name: 'a/b', revisions: ['HEAD'] },
			{ name: 'c/d', revisions: ['main', 'v1'] }
		]);
		expect(formatRepositoryLines(repos)).toBe('a/b\nc/d@main,v1');
	});
});

const wasmPath = new URL('../../../static/query.wasm', import.meta.url);

describe.skipIf(!existsSync(wasmPath))('WASM query parser', () => {
	it('highlights and validates queries', async () => {
		const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
		const p = new QueryParser(instance);
		const a = p.analyze('repo:x -file:y foo', 'keyword');
		expect(a.tokens.map((t) => t.kind)).toEqual(['field', 'value', 'negation', 'field', 'value', 'pattern']);
		expect(a.diagnostics).toEqual([]);
		const bad = p.analyze('ü case:maybe', 'keyword');
		expect(bad.diagnostics).toHaveLength(1);
		expect(bad.diagnostics[0]?.start).toBe(2);
		expect(p.analyze('patterntype:regexp x', 'keyword').patternType).toBe('regexp');
		// Pattern text in parentheses, and a grouped or among terms.
		expect(p.analyze('copyright (c) 2005', 'standard').diagnostics).toEqual([]);
		const unclear = p.analyze('a (b or c) d', 'standard');
		expect(unclear.diagnostics[0]?.message).toContain('trouble understanding');
		expect(p.analyze('a (b or c) d', 'keyword').diagnostics).toEqual([]);
		// A `)` that closes no group; quoted literals in regexp queries.
		const stray = p.analyze('x()(y or z)', 'standard');
		expect(stray.diagnostics[0]?.message).toContain('unsupported expression');
		expect(p.analyze('if (x', 'standard').diagnostics).toEqual([]);
		const quoted = p.analyze('foo "a.b("', 'regexp');
		expect(quoted.diagnostics).toEqual([]);
		expect(quoted.tokens.map((t) => t.kind)).toEqual(['pattern', 'quoted']);
		expect(p.fields()[0]).toEqual(['repo', 'r']);
		// Repeated calls reuse the buffers.
		for (let i = 0; i < 100; i++) p.analyze('lang:go '.repeat(i), 'keyword');
	});
});
