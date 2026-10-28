import { describe, expect, it } from 'vitest';
import { detectFormatting, parseJsonc, setAt } from './jsonc-edit.ts';

const doc = `{
  // The URL users reach us at.
  "externalURL": "https://old.example.com",
  /* Sign-in */
  "auth.providers": [
    { "type": "builtin", "allowSignup": false }, // keep this one
  ],
  "auth.public": false,
}
`;

describe('setAt', () => {
	it('replaces one value and keeps comments and trailing commas', () => {
		const next = setAt(doc, ['externalURL'], 'https://sourcegull.example.com');
		expect(next).toBe(doc.replace('https://old.example.com', 'https://sourcegull.example.com'));
	});

	it('adds a new key without touching the rest', () => {
		const next = setAt(doc, ['auth.sessionExpiry'], '7d');
		expect(next).toContain('// The URL users reach us at.');
		expect(next).toContain('/* Sign-in */');
		expect(next).toContain('// keep this one');
		expect(parseJsonc(next)?.['auth.sessionExpiry']).toBe('7d');
	});

	it('removes a key (reset to default) and keeps other comments', () => {
		const next = setAt(doc, ['auth.public'], undefined);
		expect(parseJsonc(next)).not.toHaveProperty('auth.public');
		expect(next).toContain('// keep this one');
		expect(next).toContain('// The URL users reach us at.');
	});

	it('edits nested values, creating parent objects', () => {
		const next = setAt(doc, ['auth.accessTokens', 'allow'], 'none');
		expect(parseJsonc(next)?.['auth.accessTokens']).toEqual({ allow: 'none' });
		const inArray = setAt(doc, ['auth.providers', 0, 'allowSignup'], true);
		expect(inArray).toContain('{ "type": "builtin", "allowSignup": true }, // keep this one');
	});

	it('appends to and removes from arrays', () => {
		const added = setAt(doc, ['auth.providers', -1], { type: 'gitlab', clientID: '', clientSecret: '' });
		const providers = parseJsonc(added)?.['auth.providers'] as { type: string }[];
		expect(providers.map((p) => p.type)).toEqual(['builtin', 'gitlab']);
		expect(added).toContain('// The URL users reach us at.');
		const removed = setAt(added, ['auth.providers', 1], undefined);
		expect((parseJsonc(removed)?.['auth.providers'] as unknown[]).length).toBe(1);
	});

	it('starts a document from empty text', () => {
		expect(parseJsonc(setAt('', ['auth.public'], true))).toEqual({ 'auth.public': true });
	});

	it('uses the document indentation', () => {
		expect(detectFormatting('{\n\t"a": 1\n}')).toMatchObject({ insertSpaces: false });
		expect(detectFormatting('{\n    "a": 1\n}')).toMatchObject({ insertSpaces: true, tabSize: 4 });
		const tabbed = setAt('{\n\t"a": 1\n}', ['b'], { c: 2 });
		expect(tabbed).toContain('\t"b": {\n\t\t"c": 2\n\t}');
	});
});

describe('parseJsonc', () => {
	it('parses comments and trailing commas, rejects broken text', () => {
		expect(parseJsonc(doc)?.['auth.public']).toBe(false);
		expect(parseJsonc('{ "a": ')).toBeNull();
		expect(parseJsonc('[1]')).toBeNull();
		expect(parseJsonc('  ')).toEqual({});
	});
});
