/**
 * Decoding of the server's compact highlighting format (`GitBlob.highlight.packed`, from
 * `sourcegull_highlight::pack`): four integers per range, `[lineDelta, start, length, kind]`.
 * `lineDelta` is relative to the previous range's line; `start` is relative to the previous
 * range's start on the same line, else absolute. Columns count UTF-16 code units, which is what
 * CodeMirror document positions use.
 */

export interface Occurrence {
	/** 0-based line. */
	line: number;
	/** 0-based start column (UTF-16). */
	startCol: number;
	/** Exclusive end column (UTF-16). */
	endCol: number;
	/** `SyntaxKind` value. */
	kind: number;
}

/** CSS class suffixes by `SyntaxKind` value (`sourcegull_highlight::SyntaxKind::css_class`). */
export const SYNTAX_KIND_CLASSES: readonly string[] = [
	'', // 0 is unused
	'comment',
	'keyword',
	'string',
	'string-escape',
	'regex',
	'number',
	'constant',
	'constant-builtin',
	'function',
	'function-builtin',
	'macro',
	'type',
	'type-builtin',
	'constructor',
	'variable-builtin',
	'parameter',
	'property',
	'operator',
	'punctuation',
	'tag',
	'attribute',
	'namespace',
	'label',
	'heading',
	'emphasis',
	'strong',
	'link',
	'code'
];

/** The CSS class of a kind (`hl-keyword`), or null for unknown kinds. */
export function kindClass(kind: number): string | null {
	const name = SYNTAX_KIND_CLASSES[kind];
	return name ? `hl-${name}` : null;
}

/** Decodes the packed array. Throws on a malformed length. */
export function decodePacked(packed: readonly number[]): Occurrence[] {
	if (packed.length % 4 !== 0) {
		throw new Error(`packed highlight length ${packed.length} is not a multiple of 4`);
	}
	const out: Occurrence[] = new Array(packed.length / 4);
	let line = 0;
	let start = 0;
	for (let i = 0; i < packed.length; i += 4) {
		const dl = packed[i]!;
		if (dl === 0) {
			start += packed[i + 1]!;
		} else {
			line += dl;
			start = packed[i + 1]!;
		}
		out[i / 4] = { line, startCol: start, endCol: start + packed[i + 2]!, kind: packed[i + 3]! };
	}
	return out;
}
