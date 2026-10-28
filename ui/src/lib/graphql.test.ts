import { describe, expect, it, vi } from 'vitest';
import { graphql, GraphQLRequestError, operationName } from './graphql.ts';
import { isConflictError } from './site-config.ts';

function jsonResponse(body: unknown, status = 200): Response {
	return new Response(JSON.stringify(body), {
		status,
		headers: { 'Content-Type': 'application/json' }
	});
}

describe('operationName', () => {
	it('extracts the first named operation', () => {
		expect(operationName('query CurrentUser { currentUser { id } }')).toBe('CurrentUser');
		expect(operationName('mutation  Save($a: Int!) { x }')).toBe('Save');
		expect(operationName('{ currentUser { id } }')).toBeUndefined();
	});
});

describe('graphql', () => {
	it('POSTs the operation with the CSRF header and returns data', async () => {
		const fetchMock = vi.fn<typeof fetch>(async () => jsonResponse({ data: { ok: 1 } }));
		const data = await graphql<{ ok: number }, { id: string }>(
			'query Thing($id: ID!) { ok }',
			{ id: 'VXNlcjox' },
			{ fetch: fetchMock }
		);
		expect(data).toEqual({ ok: 1 });

		const [url, init] = fetchMock.mock.calls[0]!;
		expect(url).toBe('/.api/graphql?Thing');
		expect(init?.method).toBe('POST');
		expect(init?.credentials).toBe('same-origin');
		const headers = init?.headers as Record<string, string>;
		expect(headers['X-Requested-With']).toBe('Sourcegull');
		expect(headers['Content-Type']).toBe('application/json');
		expect(JSON.parse(init?.body as string)).toEqual({
			query: 'query Thing($id: ID!) { ok }',
			variables: { id: 'VXNlcjox' },
			operationName: 'Thing'
		});
	});

	it('throws GraphQLRequestError carrying the GraphQL errors', async () => {
		const fetchMock = vi.fn<typeof fetch>(async () =>
			jsonResponse({
				data: null,
				errors: [{ message: 'must be site admin', extensions: { code: 'FORBIDDEN' } }]
			})
		);
		const err = await graphql('query X { site { id } }', undefined, { fetch: fetchMock }).catch(
			(e: unknown) => e
		);
		expect(err).toBeInstanceOf(GraphQLRequestError);
		const gqlErr = err as GraphQLRequestError;
		expect(gqlErr.message).toBe('must be site admin');
		expect(gqlErr.hasCode('FORBIDDEN')).toBe(true);
		expect(gqlErr.status).toBe(200);
	});

	it('reports HTTP failures without a GraphQL body', async () => {
		const fetchMock = vi.fn<typeof fetch>(
			async () => new Response('bad gateway', { status: 502, statusText: 'Bad Gateway' })
		);
		await expect(graphql('{ x }', undefined, { fetch: fetchMock })).rejects.toMatchObject({
			name: 'GraphQLRequestError',
			status: 502
		});
	});

	it('wraps network errors', async () => {
		const fetchMock = vi.fn<typeof fetch>(async () => {
			throw new TypeError('Failed to fetch');
		});
		await expect(graphql('{ x }', undefined, { fetch: fetchMock })).rejects.toMatchObject({
			status: 0
		});
	});
});

describe('isConflictError', () => {
	it('recognises conflicts by code, status or message', () => {
		expect(isConflictError(new GraphQLRequestError('x', 200, [{ message: 'x', extensions: { code: 'CONFLICT' } }]))).toBe(true);
		expect(isConflictError(new GraphQLRequestError('x', 409))).toBe(true);
		expect(
			isConflictError(
				new GraphQLRequestError('m', 200, [
					{ message: 'site configuration was modified since lastID 3' }
				])
			)
		).toBe(true);
		expect(isConflictError(new GraphQLRequestError('m', 200, [{ message: 'invalid JSON at line 2' }]))).toBe(false);
		expect(isConflictError(new Error('conflict'))).toBe(false);
	});
});
