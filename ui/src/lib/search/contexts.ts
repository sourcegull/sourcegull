/** Saving search contexts from the context form. */
import { graphql } from '#lib/graphql.ts';
import type {
	CreateSearchContextMutation,
	CreateSearchContextMutationVariables,
	RepositoryIdQueryVariables,
	UpdateSearchContextMutation,
	UpdateSearchContextMutationVariables
} from '#lib/graphql-types.ts';
import { parseRepositoryLines, type ContextFormValue } from './context-form.ts';
import {
	CREATE_SEARCH_CONTEXT,
	REPOSITORY_ID,
	UPDATE_SEARCH_CONTEXT,
	type RepositoryIDResult,
	type RepositoryRevisionsInput
} from './queries.ts';

/** Resolves repository names to IDs; throws naming the ones that do not exist. */
export async function resolveRepositories(text: string): Promise<RepositoryRevisionsInput[]> {
	const lines = parseRepositoryLines(text);
	const found = await Promise.all(
		lines.map((l) => graphql<RepositoryIDResult, RepositoryIdQueryVariables>(REPOSITORY_ID, { name: l.name }))
	);
	const missing = lines.filter((_, i) => !found[i]?.repository).map((l) => l.name);
	if (missing.length > 0) throw new Error(`Unknown repositories: ${missing.join(', ')}`);
	return lines.map((l, i) => ({ repositoryID: found[i]?.repository?.id ?? '', revisions: l.revisions }));
}

async function parts(v: ContextFormValue) {
	const repositories = v.kind === 'repositories' ? await resolveRepositories(v.repositories) : [];
	if (v.kind === 'repositories' && repositories.length === 0) {
		throw new Error('Add at least one repository.');
	}
	return { query: v.kind === 'query' ? v.query.trim() : '', repositories };
}

/** Creates a context and returns its spec. */
export async function createContext(v: ContextFormValue): Promise<string> {
	const { query, repositories } = await parts(v);
	const r = await graphql<CreateSearchContextMutation, CreateSearchContextMutationVariables>(CREATE_SEARCH_CONTEXT, {
		searchContext: {
			name: v.name,
			description: v.description,
			public: v.public,
			namespace: v.namespace,
			query
		},
		repositories
	});
	return r.createSearchContext.spec;
}

/** Updates a context and returns its (possibly renamed) spec. */
export async function updateContext(id: string, v: ContextFormValue): Promise<string> {
	const { query, repositories } = await parts(v);
	const r = await graphql<UpdateSearchContextMutation, UpdateSearchContextMutationVariables>(UPDATE_SEARCH_CONTEXT, {
		id,
		searchContext: { name: v.name, description: v.description, public: v.public, query },
		repositories
	});
	return r.updateSearchContext.spec;
}
