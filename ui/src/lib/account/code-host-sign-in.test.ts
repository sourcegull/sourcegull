import { describe, expect, it } from 'vitest';
import { signInPrompts } from './code-host-sign-in.ts';

describe('code host sign-in prompts', () => {
	it('asks to sign in again with the code host', () => {
		const prompts = signInPrompts(
			[
				{
					serviceType: 'github',
					serviceID: 'https://github.com/',
					displayName: 'GitHub',
					reason: 'MISSING_SCOPE',
					authenticationURL: '/.auth/github/login?pc=abc'
				},
				{
					serviceType: 'gitea',
					serviceID: 'https://gitea.example.com/',
					displayName: 'Gitea',
					reason: 'EXPIRED',
					authenticationURL: null
				}
			],
			'/search?q=x'
		);
		expect(prompts).toEqual([
			{
				key: 'github https://github.com/',
				message:
					'Sourcegull needs more access to your GitHub account. Sign in again with GitHub to see your private repositories.',
				href: '/.auth/github/login?pc=abc&returnTo=%2Fsearch%3Fq%3Dx',
				linkText: 'Sign in with GitHub'
			},
			{
				key: 'gitea https://gitea.example.com/',
				message:
					'Your Gitea sign-in has expired. Sign in again with Gitea to see your private repositories.',
				href: null,
				linkText: 'Sign in with Gitea'
			}
		]);
	});

	it('shows nothing when nothing is needed', () => {
		expect(signInPrompts([], '/')).toEqual([]);
	});
});
