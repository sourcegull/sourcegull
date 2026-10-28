/**
 * Resolving a symbol block to the symbol's current definition: a symbol
 * search in the block's repository and revision, first in the recorded file, then anywhere in
 * the repository (the symbol moved), matching name, kind and container.
 */
import { exactRepo } from '#lib/search/query.ts';
import type { SymbolInfo, SymbolMatch } from '#lib/search/stream.ts';
import type { SymbolBlockInput } from './serialize.ts';

function escapeRegexp(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** A regexp-pattern query for the symbol (`inFile`: only in the recorded file). */
export function symbolSearchQuery(s: SymbolBlockInput, inFile: boolean): string {
	const repo = exactRepo(s.repositoryName) + (s.revision ? `@${s.revision}` : '');
	const file = inFile ? ` file:^${escapeRegexp(s.filePath)}$` : '';
	return `repo:${repo}${file} type:symbol count:100 ^${escapeRegexp(s.symbolName)}$`;
}

export interface FoundSymbol {
	/** The commit searched. */
	commit: string;
	path: string;
	/** 1-based line and columns of the name (columns as in the match's URL). */
	line: number;
	startCol: number;
	endCol: number;
}

/** `#L<line>:<col>-<line>:<col>` of a symbol URL, 1-based. */
export function symbolURLRange(url: string): { line: number; startCol: number; endCol: number } | null {
	const m = /#L(\d+):(\d+)-(\d+):(\d+)$/.exec(url);
	if (!m) return null;
	return { line: Number(m[1]), startCol: Number(m[2]), endCol: Number(m[4]) };
}

/**
 * The best match for the block among `matches`: same name, kind and container; else same name
 * and kind; else same name. Ties go to the recorded file.
 */
export function pickSymbol(s: SymbolBlockInput, matches: readonly SymbolMatch[]): FoundSymbol | null {
	const candidates: { m: SymbolMatch; sym: SymbolInfo; score: number }[] = [];
	for (const m of matches) {
		for (const sym of m.symbols) {
			if (sym.name !== s.symbolName) continue;
			let score = 1;
			if (s.symbolKind === 'UNKNOWN' || sym.kind === s.symbolKind) score += 2;
			if (sym.containerName === s.symbolContainerName) score += 1;
			if (m.path === s.filePath) score += 4;
			candidates.push({ m, sym, score });
		}
	}
	candidates.sort((a, b) => b.score - a.score);
	const best = candidates[0];
	if (!best) return null;
	const range = symbolURLRange(best.sym.url);
	return {
		commit: best.m.commit,
		path: best.m.path,
		line: range?.line ?? best.sym.line,
		startCol: range?.startCol ?? 1,
		endCol: range?.endCol ?? 1
	};
}
