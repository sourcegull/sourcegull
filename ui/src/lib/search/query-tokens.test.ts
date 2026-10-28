import { describe, expect, it } from 'vitest';
import { queryTokens } from './query-tokens.ts';

describe('queryTokens', () => {
	it('splits fields, quoted values, parens and keywords', () => {
		const t = queryTokens('-repo:foo type:commit after:"1 month ago" (a OR b)');
		expect(t.map((x) => x.text).join('')).toBe('-repo:foo type:commit after:"1 month ago" (a OR b)');
		expect(t.filter((x) => x.kind === 'field').map((x) => x.text)).toEqual(['repo:', 'type:', 'after:']);
		expect(t.find((x) => x.kind === 'negation')?.text).toBe('-');
		expect(t.find((x) => x.kind === 'quoted')?.text).toBe('"1 month ago"');
		expect(t.find((x) => x.kind === 'keyword')?.text).toBe('OR');
		expect(t.filter((x) => x.kind === 'paren')).toHaveLength(2);
	});

	it('keeps plain text and predicates intact', () => {
		const t = queryTokens('repo:has.file(path:go.mod) http.Handle');
		expect(t.map((x) => x.text).join('')).toBe('repo:has.file(path:go.mod) http.Handle');
		expect(t[0]).toEqual({ kind: 'field', text: 'repo:' });
	});
});
