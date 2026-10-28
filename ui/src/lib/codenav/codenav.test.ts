import { describe, expect, it } from 'vitest';
import { wordAt } from '#lib/editor/codenav.ts';
import { inlineSpans, parseBlocks, unescapeText } from './markdown.ts';
import { provenanceLabel } from './queries.ts';

describe('hover markdown', () => {
	it('splits code fences, rules and paragraphs', () => {
		expect(parseBlocks('```go\nfunc F() {\n```\n\n---\n\nDoes F.\nMore.\n\nNext')).toEqual([
			{ kind: 'code', lang: 'go', text: 'func F() {' },
			{ kind: 'rule' },
			{ kind: 'para', text: 'Does F.\nMore.' },
			{ kind: 'para', text: 'Next' }
		]);
	});
	it('keeps longer fences and unterminated blocks', () => {
		expect(parseBlocks('````\na ``` b\n````')).toEqual([{ kind: 'code', lang: '', text: 'a ``` b' }]);
		expect(parseBlocks('```\nopen')).toEqual([{ kind: 'code', lang: '', text: 'open' }]);
		expect(parseBlocks('<b>x</b>')).toEqual([{ kind: 'para', text: '<b>x</b>' }]);
	});

	it('resolves escapes and character references outside code', () => {
		// scip-python's docstrings: escaped underscores and &nbsp; indentation.
		expect(unescapeText(':param secret\\_key: one&nbsp;&nbsp;two &amp; &#39;x&#x27; &bogus; \\a')).toBe(
			":param secret_key: one\u00a0\u00a0two & 'x' &bogus; \\a"
		);
		expect(unescapeText('&#0; &#xD800;')).toBe('\ufffd \ufffd');
		expect(inlineSpans('a\\_b `c\\_d &amp;` e&lt;')).toEqual([
			{ code: false, text: 'a_b ' },
			{ code: true, text: 'c\\_d &amp;' },
			{ code: false, text: ' e<' }
		]);
	});
});

describe('identifiers', () => {
	it('finds the word around an offset', () => {
		const line = '  let fooBar = baz_2(x);';
		expect(wordAt(line, 7)).toEqual([6, 12]);
		expect(wordAt(line, 12)).toEqual([6, 12]);
		expect(wordAt(line, 1)).toBeNull();
		expect(wordAt('x = 42', 5)).toBeNull();
		expect(wordAt('naïve()', 2)).toEqual([0, 5]);
		expect(wordAt('$el.x', 1)).toEqual([0, 3]);
	});
	it('labels provenance', () => {
		expect(provenanceLabel('SEARCH_BASED')).toBe('Search-based');
		expect(provenanceLabel('PRECISE')).toBe('Precise');
	});
});
