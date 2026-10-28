import { describe, expect, it } from 'vitest';
import {
	giteaApplicationURLs,
	githubOAuthAppURLs,
	gitlabApplicationURLs,
	gitlabBase,
	isHttpURL,
	isSupportedProviderType,
	missingRequired,
	newProvider,
	providerTypeInfo,
	providerVariant,
	redirectURI
} from './auth-providers.ts';
import type { JsonSchema } from './site-config-form.ts';

describe('provider types', () => {
	it('marks SAML and unknown types as unsupported', () => {
		expect(isSupportedProviderType('openidconnect')).toBe(true);
		expect(isSupportedProviderType('saml')).toBe(false);
		expect(isSupportedProviderType('cas')).toBe(false);
		expect(providerTypeInfo('saml')).toMatchObject({ label: 'SAML', addable: false });
	});
});

describe('redirect URIs', () => {
	it('builds the callback from externalURL', () => {
		expect(redirectURI('https://sourcegull.example.com', 'gitlab')).toBe('https://sourcegull.example.com/.auth/gitlab/callback');
		expect(redirectURI('https://sourcegull.example.com/', 'gitlab')).toBe('https://sourcegull.example.com/.auth/gitlab/callback');
		expect(redirectURI(' https://x.test/sub// ', 'openidconnect')).toBe('https://x.test/sub/.auth/openidconnect/callback');
	});

	it('needs a valid http(s) externalURL', () => {
		expect(redirectURI(undefined, 'gitlab')).toBeNull();
		expect(redirectURI('', 'gitlab')).toBeNull();
		expect(redirectURI('sourcegull.example.com', 'gitlab')).toBeNull();
		expect(redirectURI('ftp://x', 'gitlab')).toBeNull();
		expect(redirectURI('https://x', 'builtin')).toBeNull();
		expect(isHttpURL('http://localhost:7080')).toBe(true);
	});
});

describe('GitLab application URLs', () => {
	it('defaults to gitlab.com and follows the provider url', () => {
		expect(gitlabBase(undefined)).toBe('https://gitlab.com');
		expect(gitlabApplicationURLs('').user).toBe('https://gitlab.com/-/user_settings/applications');
		const apps = gitlabApplicationURLs('https://gitlab.example.com/');
		expect(apps.admin).toBe('https://gitlab.example.com/admin/applications');
		expect(apps.user).toBe('https://gitlab.example.com/-/user_settings/applications');
	});
});

describe('GitHub and Gitea application URLs', () => {
	it('defaults GitHub to github.com and needs a Gitea URL', () => {
		expect(githubOAuthAppURLs(undefined).user).toBe('https://github.com/settings/applications/new');
		expect(githubOAuthAppURLs('https://ghe.example.com/').org).toBe(
			'https://ghe.example.com/organizations/<org>/settings/applications/new'
		);
		expect(giteaApplicationURLs('')).toBeNull();
		expect(giteaApplicationURLs('https://gitea.example.com/')).toEqual({
			user: 'https://gitea.example.com/user/settings/applications',
			admin: 'https://gitea.example.com/-/admin/applications'
		});
		expect(redirectURI('https://sourcegull.example.com', 'github')).toBe('https://sourcegull.example.com/.auth/github/callback');
		expect(redirectURI('https://sourcegull.example.com', 'gitea')).toBe('https://sourcegull.example.com/.auth/gitea/callback');
	});
});

describe('providers', () => {
	const schema: JsonSchema = {
		definitions: {
			AuthProvider: {
				oneOf: [
					{ properties: { type: { enum: ['builtin'] } }, required: ['type'] },
					{
						properties: { type: { enum: ['gitlab'] }, clientID: { type: 'string' }, clientSecret: { type: 'string' } },
						required: ['clientID', 'clientSecret', 'type']
					}
				]
			}
		}
	};

	it('finds the schema variant and the missing required fields', () => {
		const variant = providerVariant(schema, 'gitlab');
		expect(variant?.required).toContain('clientID');
		const p = newProvider('gitlab');
		expect(p).toMatchObject({ type: 'gitlab', url: 'https://gitlab.com/' });
		expect(missingRequired(variant, p)).toEqual(['clientID', 'clientSecret']);
		expect(missingRequired(variant, { ...p, clientID: 'id', clientSecret: 's' })).toEqual([]);
		expect(providerVariant(schema, 'nope')).toBeNull();
	});
});
