import { describe, expect, it } from 'vitest';
import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
import {
	appFieldEdit,
	authMethodEdits,
	classicTokenURL,
	exclusionsValue,
	fineGrainedTokenURL,
	jsonOnlyKeys,
	normalizeOrg,
	newAppURL,
	normalizeRepo,
	readApp,
	readAuthMethod,
	readExclusions,
	readRepos,
	repoEdits
} from './github-form.ts';
import type { Edit } from './gitlab-form.ts';
import { DEFAULT_GITHUB_CONFIG, defaultConfig, kindLabel, parseKind } from './queries.ts';

function apply(text: string, edits: Edit[]): string {
	return edits.reduce((t, [path, value]) => setAt(t, path, value), text);
}

describe('GitHub connection form', () => {
	it('reads the default configuration as affiliated', () => {
		const doc = parseJsonc(DEFAULT_GITHUB_CONFIG)!;
		expect(readRepos(doc).mode).toBe('affiliated');
		expect(jsonOnlyKeys(doc)).toEqual([]);
	});

	it('reads orgs and repos as selected, and queries as custom', () => {
		expect(readRepos({ orgs: ['acme'], repos: ['x/y'] })).toEqual({
			mode: 'selected',
			orgs: ['acme'],
			repos: ['x/y'],
			queries: []
		});
		expect(readRepos({ repositoryQuery: ['affiliated'], repos: ['x/y'] }).mode).toBe('custom');
		expect(readRepos({ repositoryQuery: ['org:acme topic:go'] }).mode).toBe('custom');
	});

	it('writes the selection and keeps comments', () => {
		const text = apply(
			DEFAULT_GITHUB_CONFIG,
			repoEdits({ mode: 'selected', orgs: ['acme'], repos: ['other/tool'], queries: [] })
		);
		const doc = parseJsonc(text)!;
		expect(doc.repositoryQuery).toBeUndefined();
		expect(doc.orgs).toEqual(['acme']);
		expect(doc.repos).toEqual(['other/tool']);
		expect(text).toContain('// The GitHub instance');
		const back = parseJsonc(apply(text, repoEdits({ mode: 'affiliated', orgs: [], repos: [], queries: [] })))!;
		expect(back.repositoryQuery).toEqual(['affiliated']);
		expect(back.orgs).toBeUndefined();
	});

	it('normalizes pasted URLs', () => {
		expect(normalizeRepo('https://github.com/acme/api/blob/main/README.md')).toBe('acme/api');
		expect(normalizeRepo('acme/api.git')).toBe('acme/api');
		expect(normalizeOrg('https://github.com/orgs/acme/repositories')).toBe('acme');
		expect(normalizeOrg('https://ghe.corp/acme/')).toBe('acme');
	});

	it('round-trips exclusions and keeps entries it does not show', () => {
		const doc = { exclude: [{ name: 'a/b' }, { archived: true }, { forks: true }, { id: 7 }, { pattern: '^x' }] };
		const ex = readExclusions(doc);
		expect(ex).toEqual({ names: ['a/b'], patterns: ['^x'], archived: true, forks: true, other: [{ id: 7 }] });
		expect(exclusionsValue(ex)).toEqual([
			{ id: 7 },
			{ name: 'a/b' },
			{ pattern: '^x' },
			{ archived: true },
			{ forks: true }
		]);
		expect(exclusionsValue({ names: [], patterns: [], archived: false, forks: false, other: [] })).toBeUndefined();
	});

	it('links the token pages of the instance', () => {
		expect(fineGrainedTokenURL('https://ghe.corp/')).toBe('https://ghe.corp/settings/personal-access-tokens/new');
		expect(classicTokenURL(undefined)).toBe(
			'https://github.com/settings/tokens/new?description=Sourcegull&scopes=repo'
		);
	});

	it('picks the kind and its starting configuration', () => {
		expect(parseKind('github')).toBe('GITHUB');
		expect(parseKind(null)).toBe('GITLAB');
		expect(parseKind('svn')).toBe('GITLAB');
		expect(kindLabel('GITHUB')).toBe('GitHub');
		expect(defaultConfig('GITHUB')).toBe(DEFAULT_GITHUB_CONFIG);
	});

	it('switches between a token and a GitHub App', () => {
		const text = '{\n  "url": "https://github.com",\n  "token": "REDACTED"\n}';
		expect(readAuthMethod(parseJsonc(text)!)).toBe('token');
		let app = apply(text, authMethodEdits('app', 'REDACTED'));
		let doc = parseJsonc(app)!;
		expect(readAuthMethod(doc)).toBe('app');
		expect(doc.token).toBeUndefined();
		app = apply(app, [
			appFieldEdit('appID', ' 123 ', false),
			appFieldEdit('installationID', '456', false),
			appFieldEdit('privateKey', '-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----', false)
		]);
		doc = parseJsonc(app)!;
		expect(doc.githubApp).toEqual({
			appID: 123,
			privateKey: '-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----\n',
			installationID: 456
		});
		expect(readApp(doc)).toEqual({
			appID: '123',
			privateKey: '-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----\n',
			installationID: '456'
		});
		expect(jsonOnlyKeys(doc)).toEqual([]);
		// Not a number, or emptied: 0, which validation rejects. An emptied key keeps the stored one.
		expect(appFieldEdit('appID', 'abc', false)).toEqual([['githubApp', 'appID'], 0]);
		expect(appFieldEdit('privateKey', '', true)).toEqual([['githubApp', 'privateKey'], 'REDACTED']);
		// Back to the token: the stored one is restored.
		doc = parseJsonc(apply(app, authMethodEdits('token', 'REDACTED')))!;
		expect(readAuthMethod(doc)).toBe('token');
		expect(doc.githubApp).toBeUndefined();
		expect(doc.token).toBe('REDACTED');
		expect(newAppURL('https://ghe.corp/')).toBe('https://ghe.corp/settings/apps/new');
	});
});
