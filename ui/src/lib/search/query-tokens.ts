/**
 * A light tokenizer for showing a query with syntax colours outside the editor (example chips,
 * the recent-searches table). It is display-only: the editor's own highlighter (QueryInput) is
 * the reference, this just picks the same colour classes for fields, operators and quoted text.
 */

export type QueryTokenKind = 'field' | 'negation' | 'keyword' | 'quoted' | 'paren' | 'text' | 'space';

export interface QueryToken {
	kind: QueryTokenKind;
	text: string;
}

const KEYWORDS = new Set(['AND', 'OR', 'NOT', 'and', 'or', 'not']);

export function queryTokens(query: string): QueryToken[] {
	const out: QueryToken[] = [];
	const re = /(\s+)|("(?:[^"\\]|\\.)*"?|'(?:[^'\\]|\\.)*'?)|([()])|([^\s()"']+)/g;
	let m: RegExpExecArray | null;
	while ((m = re.exec(query)) !== null) {
		if (m[1]) out.push({ kind: 'space', text: m[1] });
		else if (m[2]) out.push({ kind: 'quoted', text: m[2] });
		else if (m[3]) out.push({ kind: 'paren', text: m[3] });
		else if (m[4]) {
			const word = m[4];
			const field = /^(-?)([a-zA-Z][a-zA-Z.]*:)(.*)$/.exec(word);
			if (KEYWORDS.has(word)) out.push({ kind: 'keyword', text: word });
			else if (field) {
				if (field[1]) out.push({ kind: 'negation', text: field[1] });
				out.push({ kind: 'field', text: field[2] ?? '' });
				if (field[3]) out.push({ kind: 'text', text: field[3] });
			} else out.push({ kind: 'text', text: word });
		}
	}
	return out;
}
