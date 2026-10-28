import { describe, expect, it, vi } from 'vitest';
import {
	authRedirect,
	builtinFormCollapsed,
	canAccessSiteAdmin,
	fetchAuthState,
	providerLogo,
	providerSignInURL,
	safeReturnTo,
	signIn,
	type AuthState
} from './auth.ts';

const anonymous: AuthState = { authenticated: false, allowSignup: true, needsSiteInit: false };
const fresh: AuthState = { authenticated: false, allowSignup: true, needsSiteInit: true };
const alice: AuthState = {
	authenticated: true,
	user: { id: 'VXNlcjox', username: 'alice', siteAdmin: false },
	allowSignup: true,
	needsSiteInit: false
};
const admin: AuthState = { ...alice, user: { id: 'VXNlcjoy', username: 'root', siteAdmin: true } };

const url = (path: string) => new URL(path, 'http://sourcegull.test');

describe('authRedirect', () => {
	it('sends anonymous visitors to sign-in with a returnTo', () => {
		expect(authRedirect(anonymous, url('/site-admin/users?x=1'))).toBe(
			'/sign-in?returnTo=%2Fsite-admin%2Fusers%3Fx%3D1'
		);
		expect(authRedirect(anonymous, url('/'))).toBe('/sign-in');
		expect(authRedirect(anonymous, url('/sign-in'))).toBeNull();
		expect(authRedirect(anonymous, url('/sign-up'))).toBeNull();
		expect(authRedirect({ ...anonymous, accessRequest: true }, url('/request-access'))).toBeNull();
		expect(authRedirect(anonymous, url('/set-password?token=x'))).toBeNull();
	});

	it('hides sign-up when sign-up is disabled', () => {
		expect(authRedirect({ ...anonymous, allowSignup: false }, url('/sign-up'))).toBe('/sign-in');
	});

	it('forces the site-init sign-up while no users exist', () => {
		expect(authRedirect(fresh, url('/search'))).toBe('/sign-up');
		expect(authRedirect(fresh, url('/sign-in'))).toBe('/sign-up');
		expect(authRedirect(fresh, url('/sign-up'))).toBeNull();
	});

	it('lets signed-in users through and bounces them off auth pages', () => {
		expect(authRedirect(alice, url('/search'))).toBeNull();
		expect(authRedirect(alice, url('/sign-in?returnTo=%2Fuser%2Fsettings%2Ftokens'))).toBe(
			'/user/settings/tokens'
		);
		expect(authRedirect(alice, url('/sign-up'))).toBe('/search');
	});
});

describe('safeReturnTo', () => {
	it('rejects external and auth-page targets', () => {
		expect(safeReturnTo('/search?q=a')).toBe('/search?q=a');
		expect(safeReturnTo('//evil.example')).toBe('/search');
		expect(safeReturnTo('/\\evil.example')).toBe('/search');
		expect(safeReturnTo('https://evil.example')).toBe('/search');
		expect(safeReturnTo('/sign-in?returnTo=/x')).toBe('/search');
		expect(safeReturnTo(null)).toBe('/search');
	});
});

describe('canAccessSiteAdmin', () => {
	it('requires an authenticated site admin', () => {
		expect(canAccessSiteAdmin(admin)).toBe(true);
		expect(canAccessSiteAdmin(alice)).toBe(false);
		expect(canAccessSiteAdmin(anonymous)).toBe(false);
	});
});

describe('auth endpoints', () => {
	it('normalises the auth-state response', async () => {
		const f = vi.fn<typeof fetch>(async () =>
			Response.json({ authenticated: false, user: { id: 'x' }, allowSignup: true })
		);
		expect(await fetchAuthState(f)).toEqual({
			authenticated: false,
			user: undefined,
			allowSignup: true,
			accessRequest: false,
			needsSiteInit: false,
			builtin: true,
			providers: [],
			batchChangesEnabled: false
		});
		expect(f.mock.calls[0]?.[0]).toBe('/-/auth-state');
	});

	it('reads batchChangesEnabled for signed-in users', async () => {
		const f = vi.fn<typeof fetch>(async () =>
			Response.json({
				authenticated: true,
				user: { id: 'VXNlcjox', username: 'alice', siteAdmin: false },
				batchChangesEnabled: true
			})
		);
		expect((await fetchAuthState(f)).batchChangesEnabled).toBe(true);
	});

	it('surfaces the server error message on failed sign-in', async () => {
		const f = vi.fn<typeof fetch>(async () =>
			Response.json({ error: 'invalid username or password' }, { status: 401 })
		);
		await expect(signIn('alice', 'wrong', f)).rejects.toMatchObject({
			message: 'invalid username or password',
			status: 401
		});
		const init = f.mock.calls[0]?.[1];
		expect(f.mock.calls[0]?.[0]).toBe('/-/sign-in');
		expect((init?.headers as Record<string, string>)['X-Requested-With']).toBe('Sourcegull');
		expect(JSON.parse(init?.body as string)).toEqual({ username: 'alice', password: 'wrong' });
	});
});

describe('providerSignInURL', () => {
	it('appends an encoded returnTo', () => {
		const p = { type: 'gitlab', displayName: 'GitLab', authenticationURL: '/.auth/gitlab/login?pc=app' };
		expect(providerSignInURL(p, '/search?q=a b')).toBe(
			'/.auth/gitlab/login?pc=app&returnTo=%2Fsearch%3Fq%3Da%20b'
		);
		expect(providerSignInURL({ ...p, authenticationURL: '/.auth/x' }, '/')).toBe('/.auth/x?returnTo=%2F');
	});
});

describe('builtinFormCollapsed', () => {
	const gitlab = { type: 'gitlab', displayName: 'GitLab', authenticationURL: '/.auth/gitlab/login?pc=app' };
	const withProvider: AuthState = { ...anonymous, builtin: true, providers: [gitlab] };

	it('collapses the password form behind external providers', () => {
		expect(builtinFormCollapsed(withProvider, url('/sign-in'))).toBe(true);
		expect(builtinFormCollapsed(withProvider, url('/sign-in?error=denied'))).toBe(true);
	});

	it('opens it for ?local=1', () => {
		expect(builtinFormCollapsed(withProvider, url('/sign-in?local=1&returnTo=%2Fsearch'))).toBe(false);
	});

	it('shows it as-is without providers, and never when builtin is off', () => {
		expect(builtinFormCollapsed({ ...anonymous, providers: [] }, url('/sign-in'))).toBe(false);
		expect(builtinFormCollapsed(anonymous, url('/sign-in'))).toBe(false);
		expect(builtinFormCollapsed({ ...withProvider, builtin: false }, url('/sign-in'))).toBe(false);
	});
});

describe('providerLogo', () => {
	it('uses the code host mark for GitLab, GitHub and Gitea and a key otherwise', () => {
		expect(providerLogo({ type: 'gitlab', displayName: 'GitLab', authenticationURL: '/x' })).toBe('gitlab');
		expect(providerLogo({ type: 'github', displayName: 'GitHub', authenticationURL: '/x' })).toBe('github');
		expect(providerLogo({ type: 'gitea', displayName: 'Gitea', authenticationURL: '/x' })).toBe('gitea');
		expect(providerLogo({ type: 'openidconnect', displayName: 'SSO', authenticationURL: '/y' })).toBe('key');
	});
});
