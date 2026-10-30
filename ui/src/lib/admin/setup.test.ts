import { describe, expect, it } from 'vitest';
import { connectionNextSteps, enforcesPermissions, hasSignInProvider, setupSteps } from './setup.ts';

const config = `{
  // comment
  "externalURL": "https://sourcegull.example.com",
  "auth.providers": [
    { "type": "builtin" },
    { "type": "gitlab", "url": "https://gitlab.example.com/", "clientID": "x", "clientSecret": "y" },
    { "type": "github", "clientID": "x", "clientSecret": "y" },
  ],
}`;

describe('setup checklist', () => {
	it('ticks the steps the configuration and counts complete', () => {
		const steps = setupSteps({ siteConfig: config, connections: 1, users: 1, emailEnabled: false });
		expect(Object.fromEntries(steps.map((s) => [s.id, s.done]))).toEqual({
			'external-url': true,
			connection: true,
			'sign-in': true,
			email: false,
			users: false
		});
	});
	it('treats built-in passwords alone as no sign-in provider', () => {
		const steps = setupSteps({ siteConfig: '{"auth.providers": [{"type": "builtin"}]}', connections: 0, users: 3, emailEnabled: true });
		expect(steps.filter((s) => !s.done).map((s) => s.id)).toEqual(['external-url', 'connection', 'sign-in']);
	});
	it('survives a missing or broken document', () => {
		expect(setupSteps({ siteConfig: null, connections: 0, users: 1, emailEnabled: false }).every((s) => !s.done)).toBe(true);
		expect(setupSteps({ siteConfig: '{', connections: 0, users: 1, emailEnabled: false })[0]?.done).toBe(false);
	});
});

describe('connection next steps', () => {
	it('finds the sign-in provider of the same host, with defaults and trailing slashes', () => {
		expect(hasSignInProvider(config, 'GITLAB', 'https://gitlab.example.com')).toBe(true);
		expect(hasSignInProvider(config, 'GITLAB', 'https://gitlab.com')).toBe(false);
		expect(hasSignInProvider(config, 'GITHUB', 'https://github.com/')).toBe(true);
		expect(hasSignInProvider(config, 'GITEA', 'https://gitea.example.com')).toBe(false);
		expect(hasSignInProvider(null, 'GITLAB', 'https://gitlab.com')).toBe(false);
	});
	it('reads the authorization block of the connection', () => {
		expect(enforcesPermissions('{"url": "x", "authorization": {"identityProvider": {"type": "oauth"}}}')).toBe(true);
		expect(enforcesPermissions('{"url": "x"} ')).toBe(false);
		expect(enforcesPermissions('{"url": "x", // "authorization": {}\n}')).toBe(false);
	});
	it('lists both steps with what is done', () => {
		const steps = connectionNextSteps(config, { kind: 'GITEA', url: 'https://gitea.example.com', config: '{}' });
		expect(steps.map((s) => [s.id, s.done])).toEqual([
			['sign-in', false],
			['permissions', false]
		]);
		expect(steps[0]?.title).toBe('Let people sign in with Gitea');
	});
});
