/**
 * GraphQL operations of the search pages: search contexts and saved searches,
 * aggregations, and repository names for autocomplete.
 */

import type {
	DefaultSearchContextQuery,
	RepositoryIdQuery,
	RepositoryNamesQuery,
	SavedSearchesQuery,
	SavedSearchQuery,
	SearchAggregateQuery,
	SearchAggregationMode,
	SearchContextBySpecQuery,
	SearchContextsQuery,
	ViewerNamespacesQuery
} from '#lib/graphql-types.ts';
import type { PatternType } from './query.ts';

// ---- Search contexts -------------------------------------------------------------------------

const CONTEXT_FIELDS = `
	id
	name
	spec
	description
	autoDefined
	query
	public
	updatedAt
	viewerCanManage
	viewerHasAsDefault
	viewerHasStarred
	managedBy
	namespace {
		__typename
		id
		... on User { username }
		... on Org { name }
	}`;

export interface NamespaceRef {
	__typename: 'User' | 'Org';
	id: string;
	username?: string;
	name?: string;
}

export interface SearchContextNode {
	id: string;
	name: string;
	spec: string;
	description: string;
	autoDefined: boolean;
	query: string;
	public: boolean;
	updatedAt: string;
	viewerCanManage: boolean;
	viewerHasAsDefault: boolean;
	viewerHasStarred: boolean;
	/** The resources file entry that owns an instance-level context; such contexts are read-only. */
	managedBy: string | null;
	namespace: NamespaceRef | null;
}

export const SEARCH_CONTEXTS = `query SearchContexts($first: Int!, $after: String, $query: String) {
	searchContexts(first: $first, after: $after, query: $query) {
		nodes {${CONTEXT_FIELDS}
		}
		totalCount
		pageInfo { endCursor hasNextPage }
	}
}`;

export type SearchContextsResult = SearchContextsQuery;

export interface SearchContextsVariables {
	first: number;
	after?: string | null;
	query?: string | null;
}

export const DEFAULT_SEARCH_CONTEXT = `query DefaultSearchContext {
	defaultSearchContext { id spec }
}`;

export type DefaultSearchContextResult = DefaultSearchContextQuery;

export const SEARCH_CONTEXT_BY_SPEC = `query SearchContextBySpec($spec: String!) {
	searchContextBySpec(spec: $spec) {${CONTEXT_FIELDS}
		repositories {
			repository { id name }
			revisions
		}
	}
}`;

export interface SearchContextDetail extends SearchContextNode {
	repositories: { repository: { id: string; name: string }; revisions: string[] }[];
}

export type SearchContextBySpecResult = SearchContextBySpecQuery;

export interface SearchContextInput {
	name: string;
	description: string;
	public: boolean;
	namespace: string | null;
	query: string;
}

export interface RepositoryRevisionsInput {
	repositoryID: string;
	revisions: string[];
}

export const CREATE_SEARCH_CONTEXT = `mutation CreateSearchContext($searchContext: SearchContextInput!, $repositories: [SearchContextRepositoryRevisionsInput!]!) {
	createSearchContext(searchContext: $searchContext, repositories: $repositories) { id spec }
}`;

export const UPDATE_SEARCH_CONTEXT = `mutation UpdateSearchContext($id: ID!, $searchContext: SearchContextEditInput!, $repositories: [SearchContextRepositoryRevisionsInput!]!) {
	updateSearchContext(id: $id, searchContext: $searchContext, repositories: $repositories) { id spec }
}`;

export const DELETE_SEARCH_CONTEXT = `mutation DeleteSearchContext($id: ID!) {
	deleteSearchContext(id: $id) { alwaysNil }
}`;

export const STAR_SEARCH_CONTEXT = `mutation StarSearchContext($searchContextID: ID!, $userID: ID!) {
	createSearchContextStar(searchContextID: $searchContextID, userID: $userID) { alwaysNil }
}`;

export const UNSTAR_SEARCH_CONTEXT = `mutation UnstarSearchContext($searchContextID: ID!, $userID: ID!) {
	deleteSearchContextStar(searchContextID: $searchContextID, userID: $userID) { alwaysNil }
}`;

export const SET_DEFAULT_SEARCH_CONTEXT = `mutation SetDefaultSearchContext($searchContextID: ID!, $userID: ID!) {
	setDefaultSearchContext(searchContextID: $searchContextID, userID: $userID) { alwaysNil }
}`;

export interface StarVariables {
	searchContextID: string;
	userID: string;
}

// ---- Saved searches --------------------------------------------------------------------------

export type SavedSearchVisibility = 'PUBLIC' | 'SECRET';

export interface SavedSearchNode {
	id: string;
	description: string;
	query: string;
	draft: boolean;
	visibility: SavedSearchVisibility;
	updatedAt: string;
	url: string;
	viewerCanAdminister: boolean;
	owner: NamespaceRef;
	createdBy: { username: string } | null;
}

export const SAVED_SEARCHES = `query SavedSearches($query: String, $owner: ID, $first: Int, $after: String, $orderBy: SavedSearchesOrderBy!) {
	savedSearches(query: $query, owner: $owner, first: $first, after: $after, orderBy: $orderBy) {
		nodes {
			id
			description
			query
			draft
			visibility
			updatedAt
			url
			viewerCanAdminister
			owner {
				__typename
				id
				... on User { username }
				... on Org { name }
			}
			createdBy { username }
		}
		totalCount
		pageInfo { endCursor hasNextPage }
	}
}`;

export type SavedSearchesOrderBy = 'SAVED_SEARCH_UPDATED_AT' | 'SAVED_SEARCH_DESCRIPTION';

export interface SavedSearchesVariables {
	query?: string | null;
	owner?: string | null;
	first: number;
	after?: string | null;
	orderBy: SavedSearchesOrderBy;
}

export type SavedSearchesResult = SavedSearchesQuery;

export const SAVED_SEARCH = `query SavedSearch($id: ID!) {
	node(id: $id) {
		__typename
		... on SavedSearch {
			id
			description
			query
			draft
			visibility
			updatedAt
			url
			viewerCanAdminister
			owner {
				__typename
				id
				... on User { username }
				... on Org { name }
			}
			createdBy { username }
		}
	}
}`;

export type SavedSearchResult = SavedSearchQuery;

export const CREATE_SAVED_SEARCH = `mutation CreateSavedSearch($input: SavedSearchInput!) {
	createSavedSearch(input: $input) { id url }
}`;

export interface CreateSavedSearchVariables {
	input: {
		owner: string;
		description: string;
		query: string;
		draft: boolean;
		visibility: SavedSearchVisibility;
	};
}

export const UPDATE_SAVED_SEARCH = `mutation UpdateSavedSearch($id: ID!, $input: SavedSearchUpdateInput!) {
	updateSavedSearch(id: $id, input: $input) { id }
}`;

export interface UpdateSavedSearchVariables {
	id: string;
	input: { description: string; query: string; draft: boolean };
}

export const DELETE_SAVED_SEARCH = `mutation DeleteSavedSearch($id: ID!) {
	deleteSavedSearch(id: $id) { alwaysNil }
}`;

export const CHANGE_SAVED_SEARCH_VISIBILITY = `mutation ChangeSavedSearchVisibility($id: ID!, $newVisibility: SavedSearchVisibility!) {
	changeSavedSearchVisibility(id: $id, newVisibility: $newVisibility) { id }
}`;

export const TRANSFER_SAVED_SEARCH = `mutation TransferSavedSearch($id: ID!, $newOwner: ID!) {
	transferSavedSearchOwnership(id: $id, newOwner: $newOwner) { id }
}`;

/** The viewer and their organizations: the namespaces they can own things in. */
export const VIEWER_NAMESPACES = `query ViewerNamespaces {
	currentUser {
		id
		username
		siteAdmin
		organizations { nodes { id name } }
	}
}`;

export type ViewerNamespacesResult = ViewerNamespacesQuery;

// ---- Aggregations ----------------------------------------------------------------------------

export type AggregationMode = SearchAggregationMode;

export const SEARCH_AGGREGATE = `query SearchAggregate($query: String!, $patternType: SearchPatternType!, $mode: SearchAggregationMode, $limit: Int!, $extendedTimeout: Boolean!) {
	searchQueryAggregate(query: $query, patternType: $patternType) {
		modeAvailability { mode available reasonUnavailable }
		aggregations(mode: $mode, limit: $limit, extendedTimeout: $extendedTimeout) {
			__typename
			... on ExhaustiveSearchAggregationResult {
				mode
				groups { label count query }
				otherResultCount
				otherGroupCount
			}
			... on NonExhaustiveSearchAggregationResult {
				mode
				groups { label count query }
				otherResultCount
				approximateOtherGroupCount
			}
			... on SearchAggregationNotAvailable { mode reason reasonType }
		}
	}
}`;

export type SearchAggregateResult = SearchAggregateQuery;
export type AggregationResult = SearchAggregateQuery['searchQueryAggregate']['aggregations'];

export interface SearchAggregateVariables {
	query: string;
	patternType: PatternType;
	mode: AggregationMode | null;
	limit: number;
	extendedTimeout: boolean;
}

// ---- Autocomplete ----------------------------------------------------------------------------

export const REPOSITORY_NAMES = `query RepositoryNames($query: String, $first: Int) {
	repositories(query: $query, first: $first) { nodes { id name } }
}`;

export type RepositoryNamesResult = RepositoryNamesQuery;

export const REPOSITORY_ID = `query RepositoryID($name: String!) {
	repository(name: $name) { id name }
}`;

export type RepositoryIDResult = RepositoryIdQuery;

/** `@user` or `org` label of a namespace. */
export function namespaceLabel(ns: NamespaceRef | null): string {
	if (!ns) return 'Instance';
	return ns.__typename === 'User' ? `@${ns.username ?? ''}` : `@${ns.name ?? ''}`;
}
