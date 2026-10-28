import { describe, expect, it } from 'vitest';
import {
	firstSentence,
	parseJsoncObject,
	sectionLine,
	sectionRows,
	sectionsOf,
	setCount,
	stripJsonc,
	type ConfigSection,
	type SiteConfigSchema
} from './site-config-sections.ts';

const schema: SiteConfigSchema = {
	properties: {
		externalURL: { description: 'The externally reachable URL. Used in links.' },
		'auth.public': { description: 'Allow anonymous users.', default: false },
		'auth.sessionExpiry': { description: 'How long a session stays valid.', default: '30d' },
		'search.limits': { default: { maxTimeoutSeconds: 60 } }
	},
	'x-sections': [
		{ id: 'general', title: 'General', description: 'URL.', keys: ['externalURL'] },
		{
			id: 'auth',
			title: 'Authentication & sign-in',
			description: 'Sign-in.',
			keys: ['auth.public', 'auth.sessionExpiry']
		},
		{ id: 'search', title: 'Search & indexing', description: 'Search.', keys: ['search.limits'] }
	]
};

const doc = `// Preamble with "quotes", and // slashes
{
  // ── Authentication & sign-in ──
  "auth.public": false, // trailing
  "auth.sessionExpiry": "7d",

  // ── Search & indexing ──
  // "search.limits": { "maxTimeoutSeconds": 120 },
  "url,]": "a // not a comment, ]",
  /* block */ "bogus": [1, 2,],
}
`;

describe('JSONC', () => {
	it('strips comments and trailing commas but not string contents', () => {
		expect(parseJsoncObject(doc)).toEqual({
			'auth.public': false,
			'auth.sessionExpiry': '7d',
			'url,]': 'a // not a comment, ]',
			bogus: [1, 2]
		});
		expect(stripJsonc('{"a": "x,}",}')).toBe('{"a": "x,}"}');
	});
	it('returns null for syntax errors and non-objects', () => {
		expect(parseJsoncObject('{')).toBeNull();
		expect(parseJsoncObject('[1]')).toBeNull();
		expect(parseJsoncObject('// only a comment')).toEqual({});
	});
});

describe('sections', () => {
	const parsed = parseJsoncObject(doc);
	const sections = sectionsOf(schema, parsed);
	const at = (i: number): ConfigSection => {
		const s = sections[i];
		if (!s) throw new Error(`no section ${i}`);
		return s;
	};

	it('adds an Other section for unknown keys', () => {
		expect(sections.map((s) => s.id)).toEqual(['general', 'auth', 'search', 'other']);
		expect(at(3).keys).toEqual(['url,]', 'bogus']);
		expect(sectionsOf(schema, {}).map((s) => s.id)).toEqual(['general', 'auth', 'search']);
	});

	it('finds where a section starts: header, else a (commented-out) key', () => {
		expect(sectionLine(doc, at(1))).toBe(3);
		expect(sectionLine(doc, at(2))).toBe(7);
		expect(sectionLine(doc, at(0))).toBeNull();
		expect(sectionLine('{\n  // "externalURL": "x",\n}', at(0))).toBe(2);
		expect(sectionLine('{\n  "auth.publicX": 1,\n  "auth.public": true\n}', at(1))).toBe(3);
	});

	it('summarizes values against defaults', () => {
		expect(sectionRows(schema, at(1), parsed)).toEqual([
			{
				key: 'auth.public',
				description: 'Allow anonymous users.',
				set: true,
				value: 'false',
				default: 'false',
				isDefault: true
			},
			{
				key: 'auth.sessionExpiry',
				description: 'How long a session stays valid.',
				set: true,
				value: '"7d"',
				default: '"30d"',
				isDefault: false
			}
		]);
		const [search] = sectionRows(schema, at(2), parsed);
		expect(search).toMatchObject({ set: false, value: null, default: '{"maxTimeoutSeconds":60}' });
		const [url] = sectionRows(schema, at(0), null);
		expect(url).toMatchObject({ set: false, default: null, description: 'The externally reachable URL.' });
		expect(setCount(at(1), parsed)).toBe(2);
		expect(setCount(at(2), parsed)).toBe(0);
	});

	it('shortens descriptions to their first sentence', () => {
		expect(firstSentence('One `a.b` thing, e.g.\nmore. Second.')).toBe('One `a.b` thing, e.g. more.');
		expect(firstSentence('No period')).toBe('No period');
		expect(firstSentence(undefined)).toBe('');
	});
});
