/** GraphQL operations of code navigation, with result types. */

import type { PageInfo } from '#lib/repo/queries.ts';
import type { SymbolInfoQuery, UsagesForSymbolQuery } from '#lib/graphql-types.ts';

export type Provenance = 'PRECISE' | 'SYNTACTIC' | 'SEARCH_BASED';
export type UsageKind = 'DEFINITION' | 'REFERENCE' | 'IMPLEMENTATION' | 'SUPER';

export interface Position {
	line: number;
	character: number;
}

export interface Range {
	start: Position;
	end: Position;
}

export const SYMBOL_INFO = `query SymbolInfo($repo: String!, $rev: String!, $path: String!, $line: Int!, $character: Int!) {
	repository(name: $repo) {
		commit(rev: $rev) {
			blob(path: $path) {
				symbolInfo(line: $line, character: $character) {
					definition { repo commit path range { line character length } }
					hover
					range { start { line character } end { line character } }
					provenance
				}
			}
		}
	}
}`;

export interface SymbolInfo {
	definition: {
		repo: string;
		commit: string;
		path: string;
		range: { line: number; character: number; length: number } | null;
	} | null;
	hover: string | null;
	range: Range;
	provenance: Provenance;
}

export type SymbolInfoResult = SymbolInfoQuery;

export const USAGES_FOR_SYMBOL = `query UsagesForSymbol($range: RangeInput!, $filter: UsagesFilter, $first: Int, $after: String) {
	usagesForSymbol(range: $range, filter: $filter, first: $first, after: $after) {
		nodes {
			symbol { name }
			provenance
			usageKind
			surroundingContent
			usageRange { repository revision path range { start { line character } end { line character } } }
		}
		pageInfo { hasNextPage endCursor }
		coverage { searchBasedRepositories { repository } crossVersionRepositories { repository version } truncated summary }
	}
}`;

export interface UsageNode {
	symbol: { name: string } | null;
	provenance: Provenance;
	usageKind: UsageKind;
	surroundingContent: string;
	usageRange: { repository: string; revision: string; path: string; range: Range };
}

/** Which repositories a precise answer covers precisely and which search-based. */
export interface UsageCoverage {
	searchBasedRepositories: { repository: string }[];
	/** Repositories whose precise usages were found at another version of the package. */
	crossVersionRepositories: { repository: string; version: string }[];
	truncated: boolean;
	summary: string;
}

export type UsagesResult = UsagesForSymbolQuery;

export interface RangeInput {
	repository: string;
	revision: string;
	path: string;
	start: Position;
	end: Position;
}

/** Short labels for provenance badges. */
export function provenanceLabel(p: Provenance): string {
	switch (p) {
		case 'PRECISE':
			return 'Precise';
		case 'SYNTACTIC':
			return 'Syntactic';
		default:
			return 'Search-based';
	}
}
