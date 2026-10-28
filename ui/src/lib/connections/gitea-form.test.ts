import { describe, expect, it } from 'vitest';
import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
import {
	exclusionsValue,
	identityProvider,
	jsonOnlyKeys,
	normalizeOrg,
	normalizeRepo,
	readExclusions,
	readRepos,
	repoEdits,
	tokenURL
} from './gitea-form.ts';
import type { Edit } from './gitlab-form.ts';
import { DEFAULT_GITEA_CONFIG, defaultConfig, kindLabel, parseKind } from './queries.ts';

function apply(text: string, edits: Edit[]): string {
	return edits.reduce((t, [path, value]) => setAt(t, path, value), text);
}

describe('Gitea connection form', () => {
	it('reads the default configuration as affiliated', () => {
		const doc = parseJsonc(DEFAULT_GITEA_CONFIG)!;
		expect(readRepos(doc).mode).toBe('affiliated');
		expect(jsonOnlyKeys(doc)).toEqual([]);
	});

	it('writes the selection and keeps comments', () => {
		const text = apply(
			DEFAULT_GITEA_CONFIG,
			repoEdits({ mode: 'selected', orgs: ['acme'], repos: ['other/tool'], queries: [] })
		);
		const doc = parseJsonc(text)!;
		expect(doc.repositoryQuery).toBeUndefined();
		expect(doc.orgs).toEqual(['acme']);
		expect(text).toContain('// The Gitea instance');
	});

	it('normalizes pasted URLs, also of an instance served under a path', () => {
		expect(normalizeRepo('https://gitea.example.com/acme/api/src/branch/main/README.md')).toBe('acme/api');
		expect(normalizeRepo('https://example.com/git/acme/api.git', 'https://example.com/git/')).toBe('acme/api');
		expect(normalizeRepo('acme/api')).toBe('acme/api');
		expect(normalizeOrg('https://example.com/git/org/acme/settings', 'https://example.com/git')).toBe('acme');
		expect(normalizeOrg('https://gitea.example.com/acme')).toBe('acme');
		expect(normalizeOrg('acme')).toBe('acme');
	});

	it('round-trips exclusions with mirrors and keeps entries it does not show', () => {
		const doc = { exclude: [{ name: 'a/b' }, { mirrors: true }, { id: 7 }, { forks: true }] };
		const ex = readExclusions(doc);
		expect(ex).toEqual({
			names: ['a/b'],
			patterns: [],
			archived: false,
			forks: true,
			mirrors: true,
			other: [{ id: 7 }]
		});
		expect(exclusionsValue(ex)).toEqual([{ id: 7 }, { name: 'a/b' }, { forks: true }, { mirrors: true }]);
		expect(exclusionsValue({ ...ex, names: [], forks: false, mirrors: false, other: [] })).toBeUndefined();
	});

	it('links the token page of the instance', () => {
		expect(tokenURL('https://example.com/git/')).toBe('https://example.com/git/user/settings/applications');
	});

	it('is offered as a kind with its starting configuration', () => {
		expect(parseKind('gitea')).toBe('GITEA');
		expect(kindLabel('GITEA')).toBe('Gitea');
		expect(defaultConfig('GITEA')).toBe(DEFAULT_GITEA_CONFIG);
	});

	it('reads the identity provider of the authorization block', () => {
		expect(identityProvider({})).toBe('none');
		expect(identityProvider({ authorization: { identityProvider: { type: 'oauth' } } })).toBe('oauth');
		expect(identityProvider({ authorization: { identityProvider: { type: 'username' } } })).toBe('username');
		expect(identityProvider({ authorization: { identityProvider: { type: 'saml' } } })).toBe('none');
		expect(jsonOnlyKeys({ authorization: { identityProvider: { type: 'oauth' } } })).toEqual([]);
	});
});
