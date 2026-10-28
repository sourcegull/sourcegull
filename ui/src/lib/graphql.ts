/**
 * A tiny GraphQL client for the Sourcegull API (`POST /.api/graphql`).
 *
 * No caching or normalisation: pages call `graphql()` directly and keep results in component state.
 */

/** Header required on cookie-authenticated non-GET requests (CSRF protection, see the runtime contract). */
export const REQUESTED_WITH_HEADER = 'X-Requested-With';
export const REQUESTED_WITH_VALUE = 'Sourcegull';

export const GRAPHQL_ENDPOINT = '/.api/graphql';

/** One entry of a GraphQL response's `errors` array. */
export interface GraphQLErrorEntry {
	message: string;
	path?: (string | number)[];
	extensions?: Record<string, unknown>;
}

interface GraphQLResponse<TData> {
	data?: TData | null;
	errors?: GraphQLErrorEntry[];
}

/** Thrown when the request fails at the HTTP level or the response carries GraphQL errors. */
export class GraphQLRequestError extends Error {
	/** HTTP status of the response (0 when the request never got a response). */
	readonly status: number;
	/** GraphQL errors returned by the server (empty for transport failures). */
	readonly errors: GraphQLErrorEntry[];

	constructor(message: string, status: number, errors: GraphQLErrorEntry[] = []) {
		super(message);
		this.name = 'GraphQLRequestError';
		this.status = status;
		this.errors = errors;
	}

	/** True if any GraphQL error has the given `extensions.code`. */
	hasCode(code: string): boolean {
		return this.errors.some((e) => e.extensions?.['code'] === code);
	}
}

export interface GraphQLOptions {
	/** Defaults to the global `fetch`; pass SvelteKit's `fetch` from a `load` function. */
	fetch?: typeof fetch;
	signal?: AbortSignal;
}

/** Returns the name of the first named operation in `query`, if any (used in the URL for server logs). */
export function operationName(query: string): string | undefined {
	return /\b(?:query|mutation|subscription)\s+([_A-Za-z][_0-9A-Za-z]*)/.exec(query)?.[1];
}

/**
 * Executes a GraphQL operation and returns its `data`.
 *
 * Rejects with {@link GraphQLRequestError} on network errors, non-2xx responses, or any GraphQL errors.
 */
export async function graphql<TData, TVariables extends object = Record<string, never>>(
	query: string,
	variables?: TVariables,
	options: GraphQLOptions = {}
): Promise<TData> {
	const doFetch = options.fetch ?? fetch;
	const name = operationName(query);
	const url = name ? `${GRAPHQL_ENDPOINT}?${encodeURIComponent(name)}` : GRAPHQL_ENDPOINT;

	let response: Response;
	try {
		response = await doFetch(url, {
			method: 'POST',
			credentials: 'same-origin',
			headers: {
				'Content-Type': 'application/json',
				Accept: 'application/json',
				[REQUESTED_WITH_HEADER]: REQUESTED_WITH_VALUE
			},
			body: JSON.stringify({ query, variables: variables ?? {}, operationName: name }),
			signal: options.signal
		});
	} catch (err) {
		if (err instanceof DOMException && err.name === 'AbortError') throw err;
		throw new GraphQLRequestError(`Network error: ${String(err)}`, 0);
	}

	let body: GraphQLResponse<TData> | undefined;
	try {
		body = (await response.json()) as GraphQLResponse<TData>;
	} catch {
		body = undefined;
	}

	const errors = body?.errors ?? [];
	if (errors.length > 0) {
		throw new GraphQLRequestError(
			errors.map((e) => e.message).join('\n'),
			response.status,
			errors
		);
	}
	if (!response.ok) {
		throw new GraphQLRequestError(
			`Request failed: ${response.status} ${response.statusText}`.trim(),
			response.status
		);
	}
	if (body?.data === undefined || body.data === null) {
		throw new GraphQLRequestError('Response contained no data', response.status);
	}
	return body.data;
}

/** A user-presentable message for any error thrown by `graphql()` or `fetch`. */
export function errorMessage(err: unknown): string {
	if (err instanceof Error) return err.message;
	return String(err);
}
