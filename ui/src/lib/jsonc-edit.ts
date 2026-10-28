/**
 * Minimal edits of a JSONC document (the site configuration as the admin wrote it): a form
 * change rewrites only the value it touches, so comments and hand formatting elsewhere survive.
 */
import { applyEdits, modify, parse, type FormattingOptions, type ParseError } from 'jsonc-parser';
import type { ConfigPath } from './site-config-form.ts';

/** Indentation and line endings as the document already uses them. */
export function detectFormatting(text: string): FormattingOptions {
	const eol = text.includes('\r\n') ? '\r\n' : '\n';
	const indent = /^([ \t]+)\S/m.exec(text)?.[1] ?? '  ';
	return indent.startsWith('\t')
		? { insertSpaces: false, tabSize: 4, eol }
		: { insertSpaces: true, tabSize: Math.min(indent.length, 8), eol };
}

/**
 * Sets the value at `path` (or removes it when `value` is undefined). An index of -1 appends
 * to an array. Missing parent objects are created.
 */
export function setAt(text: string, path: ConfigPath, value: unknown): string {
	const source = text.trim() === '' ? '{}' : text;
	const edits = modify(source, path, value, { formattingOptions: detectFormatting(source) });
	return applyEdits(source, edits);
}

/** Parses JSONC (comments, trailing commas). Null when the text has a syntax error. */
export function parseJsonc(text: string): Record<string, unknown> | null {
	if (text.trim() === '') return {};
	const errors: ParseError[] = [];
	const value: unknown = parse(text, errors, { allowTrailingComma: true, disallowComments: false });
	if (errors.length || !value || typeof value !== 'object' || Array.isArray(value)) return null;
	return value as Record<string, unknown>;
}
