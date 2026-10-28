/**
 * GraphQL operations for notebooks, and the conversion between its blocks and the UI's.
 */
import type { Block } from './serialize.ts';
import type { NotebookQuery, NotebooksQuery } from '#lib/graphql-types.ts';

export interface NamespaceRef {
	__typename: 'User' | 'Org';
	id: string;
	url: string;
	username?: string;
	name?: string;
}

interface FileInputNode {
	repositoryName: string;
	filePath: string;
	revision: string | null;
	lineRange: { startLine: number; endLine: number } | null;
}

interface SymbolInputNode {
	repositoryName: string;
	filePath: string;
	revision: string | null;
	lineContext: number;
	symbolName: string;
	symbolContainerName: string;
	symbolKind: string;
}

export type BlockNode =
	| { __typename: 'MarkdownBlock'; id: string; markdownInput: string }
	| { __typename: 'QueryBlock'; id: string; queryInput: string }
	| { __typename: 'FileBlock'; id: string; fileInput: FileInputNode }
	| { __typename: 'SymbolBlock'; id: string; symbolInput: SymbolInputNode };

export interface NotebookSummary {
	id: string;
	title: string;
	public: boolean;
	updatedAt: string;
	createdAt: string;
	viewerHasStarred: boolean;
	creator: { username: string } | null;
	namespace: NamespaceRef | null;
	stars: { totalCount: number };
}

export interface NotebookNode extends NotebookSummary {
	viewerCanManage: boolean;
	patternType: string;
	updater: { username: string } | null;
	blocks: BlockNode[];
}

const NAMESPACE_FIELDS = `namespace { __typename id url ... on User { username } ... on Org { name } }`;

const SUMMARY_FIELDS = `id title public updatedAt createdAt viewerHasStarred
	creator { username }
	${NAMESPACE_FIELDS}
	stars(first: 0) { totalCount }`;

const NOTEBOOK_FIELDS = `${SUMMARY_FIELDS}
	viewerCanManage patternType
	updater { username }
	blocks {
		__typename
		... on MarkdownBlock { id markdownInput }
		... on QueryBlock { id queryInput }
		... on FileBlock { id fileInput { repositoryName filePath revision lineRange { startLine endLine } } }
		... on SymbolBlock { id symbolInput { repositoryName filePath revision lineContext symbolName symbolContainerName symbolKind } }
	}`;

export type NotebooksOrderBy = 'NOTEBOOK_UPDATED_AT' | 'NOTEBOOK_CREATED_AT' | 'NOTEBOOK_STAR_COUNT';

export const NOTEBOOKS = `query Notebooks($first: Int!, $after: String, $query: String, $creatorUserID: ID,
		$starredByUserID: ID, $namespace: ID, $orderBy: NotebooksOrderBy, $descending: Boolean) {
	notebooks(first: $first, after: $after, query: $query, creatorUserID: $creatorUserID,
			starredByUserID: $starredByUserID, namespace: $namespace, orderBy: $orderBy, descending: $descending) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { ${SUMMARY_FIELDS} }
	}
}`;

export interface NotebooksVariables {
	first: number;
	after?: string | null;
	query?: string | null;
	creatorUserID?: string | null;
	starredByUserID?: string | null;
	namespace?: string | null;
	orderBy?: NotebooksOrderBy;
	descending?: boolean;
}

export type NotebooksResult = NotebooksQuery;

export const NOTEBOOK = `query Notebook($id: ID!) {
	notebook(id: $id) { ${NOTEBOOK_FIELDS} }
}`;

export type NotebookResult = NotebookQuery;

export const CREATE_NOTEBOOK = `mutation CreateNotebook($notebook: NotebookInput!) {
	createNotebook(notebook: $notebook) { id }
}`;

export const UPDATE_NOTEBOOK = `mutation UpdateNotebook($id: ID!, $notebook: NotebookInput!) {
	updateNotebook(id: $id, notebook: $notebook) { ${NOTEBOOK_FIELDS} }
}`;

export const DELETE_NOTEBOOK = `mutation DeleteNotebook($id: ID!) {
	deleteNotebook(id: $id) { alwaysNil }
}`;

export const CREATE_NOTEBOOK_STAR = `mutation CreateNotebookStar($id: ID!) {
	createNotebookStar(notebookID: $id) { createdAt }
}`;

export const DELETE_NOTEBOOK_STAR = `mutation DeleteNotebookStar($id: ID!) {
	deleteNotebookStar(notebookID: $id) { alwaysNil }
}`;

/** The GraphQL `CreateNotebookBlockInput`. */
export type BlockInputVariable =
	| { id: string; type: 'MARKDOWN'; markdownInput: string }
	| { id: string; type: 'QUERY'; queryInput: string }
	| { id: string; type: 'FILE'; fileInput: FileInputNode }
	| { id: string; type: 'SYMBOL'; symbolInput: SymbolInputNode };

export interface NotebookInputVariable {
	title: string;
	namespace: string;
	public: boolean;
	blocks: BlockInputVariable[];
}

/** A UI block as a GraphQL block input. */
export function blockToInput(b: Block): BlockInputVariable {
	switch (b.type) {
		case 'md':
			return { id: b.id, type: 'MARKDOWN', markdownInput: b.input.text };
		case 'query':
			return { id: b.id, type: 'QUERY', queryInput: b.input.query };
		case 'file':
			return {
				id: b.id,
				type: 'FILE',
				fileInput: {
					repositoryName: b.input.repositoryName,
					revision: b.input.revision || null,
					filePath: b.input.filePath,
					lineRange: b.input.lineRange
				}
			};
		case 'symbol':
			return {
				id: b.id,
				type: 'SYMBOL',
				symbolInput: { ...b.input, revision: b.input.revision || null, symbolKind: symbolKind(b.input.symbolKind) }
			};
	}
}

/** A valid GraphQL `SymbolKind` for `kind` (imported files may hold anything). */
function symbolKind(kind: string): string {
	const k = kind.toUpperCase();
	return (SYMBOL_KINDS as readonly string[]).includes(k) ? k : 'UNKNOWN';
}

/** A GraphQL block as a UI block. */
export function inputToBlock(n: BlockNode): Block {
	switch (n.__typename) {
		case 'MarkdownBlock':
			return { id: n.id, type: 'md', input: { text: n.markdownInput } };
		case 'QueryBlock':
			return { id: n.id, type: 'query', input: { query: n.queryInput } };
		case 'FileBlock':
			return {
				id: n.id,
				type: 'file',
				input: {
					repositoryName: n.fileInput.repositoryName,
					revision: n.fileInput.revision ?? '',
					filePath: n.fileInput.filePath,
					lineRange: n.fileInput.lineRange
						? { startLine: n.fileInput.lineRange.startLine, endLine: n.fileInput.lineRange.endLine }
						: null
				}
			};
		case 'SymbolBlock':
			return {
				id: n.id,
				type: 'symbol',
				input: {
					repositoryName: n.symbolInput.repositoryName,
					revision: n.symbolInput.revision ?? '',
					filePath: n.symbolInput.filePath,
					symbolName: n.symbolInput.symbolName,
					symbolContainerName: n.symbolInput.symbolContainerName,
					symbolKind: n.symbolInput.symbolKind,
					lineContext: n.symbolInput.lineContext
				}
			};
	}
}

/** `@user` or `@org` of a notebook's namespace. */
export function namespaceLabel(ns: NamespaceRef | null): string {
	if (!ns) return '(deleted)';
	return `@${ns.username ?? ns.name ?? ''}`;
}

/** The page of a notebook. */
export function notebookURL(id: string): string {
	return `/notebooks/${encodeURIComponent(id)}`;
}

/** A new random block ID. */
export function newBlockID(): string {
	return globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/** The symbol kinds of the GraphQL `SymbolKind` enum. */
export const SYMBOL_KINDS = [
	'UNKNOWN',
	'FILE',
	'MODULE',
	'NAMESPACE',
	'PACKAGE',
	'CLASS',
	'METHOD',
	'PROPERTY',
	'FIELD',
	'CONSTRUCTOR',
	'ENUM',
	'INTERFACE',
	'FUNCTION',
	'VARIABLE',
	'CONSTANT',
	'STRING',
	'NUMBER',
	'BOOLEAN',
	'ARRAY',
	'OBJECT',
	'KEY',
	'NULL',
	'ENUMMEMBER',
	'STRUCT',
	'EVENT',
	'OPERATOR',
	'TYPEPARAMETER'
] as const;
