import { describe, expect, it } from 'vitest';
import { answerAuthorization, AuthorizationError, authorizeURL, clientLabel } from './oauth.ts';

describe('clientLabel', () => {
	it('prefers the name, then the metadata document host, then the redirect host', () => {
		expect(clientLabel({ clientName: ' Agent ', clientID: 'sourcegull-x' })).toBe('Agent');
		expect(clientLabel({ clientName: '', clientID: 'https://agent.example.com/client.json' })).toBe(
			'agent.example.com'
		);
		expect(clientLabel({ clientName: '', clientID: 'sourcegull-x', redirectHost: '127.0.0.1' })).toBe('App on 127.0.0.1');
		expect(clientLabel({ clientName: '', clientID: 'sourcegull-x', redirectHosts: [] })).toBe('Unnamed app');
	});
});

describe('answerAuthorization', () => {
	it('sends the CSRF header and returns the redirect URL', async () => {
		let init: RequestInit | undefined;
		const f = (async (_: string, i: RequestInit) => {
			init = i;
			return new Response(JSON.stringify({ redirectURL: 'http://127.0.0.1:1/cb?code=c' }));
		}) as unknown as typeof fetch;
		expect(await answerAuthorization('r', true, f)).toBe('http://127.0.0.1:1/cb?code=c');
		expect((init?.headers as Record<string, string>)['X-Requested-With']).toBe('Sourcegull');
		expect(init?.body).toBe('{"approve":true}');
	});

	it('throws the server message', async () => {
		const f = (async () =>
			new Response(JSON.stringify({ error: 'expired', code: 'NOT_FOUND' }), { status: 404 })) as unknown as typeof fetch;
		await expect(answerAuthorization('r', false, f)).rejects.toEqual(new AuthorizationError('expired', 404));
	});
});

describe('authorizeURL', () => {
	it('goes back to the authorization endpoint with the request', () => {
		expect(authorizeURL('client_id=https%3A%2F%2Fa.example.com%2Fc.json&state=x')).toBe(
			'/.oauth/authorize?client_id=https%3A%2F%2Fa.example.com%2Fc.json&state=x'
		);
		expect(authorizeURL(null)).toBeNull();
		expect(authorizeURL('state=x')).toBeNull();
	});
});
