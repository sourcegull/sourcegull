/**
 * Sections of the site configuration editor: the sections and per-key defaults come from the
 * site config JSON Schema (`x-sections`, generated in `crates/config`), the current values from
 * the document being edited.
 */

/** Where the server serves the site config JSON Schema. */
export const SITE_CONFIG_SCHEMA_URL = '/.api/site-config-schema';

/** One group of related keys. */
export interface ConfigSection {
	id: string;
	title: string;
	description: string;
	keys: string[];
}

/** The subset of the schema this module reads. */
export interface SiteConfigSchema {
	properties?: Record<string, { description?: string; default?: unknown }>;
	'x-sections'?: ConfigSection[];
}

/** One key of a section, as the summary shows it. */
export interface SectionRow {
	key: string;
	/** First sentence of the key's description. */
	description: string;
	/** Whether the document sets the key. */
	set: boolean;
	/** The value in the document, as compact JSON, or null when unset. */
	value: string | null;
	/** The default, as compact JSON, or null when the key has none. */
	default: string | null;
	/** Whether the document sets the key to its default. */
	isDefault: boolean;
}

/** The section for keys the schema does not know. */
export const OTHER_SECTION: ConfigSection = {
	id: 'other',
	title: 'Other',
	description: 'Keys this version does not know.',
	keys: []
};

/** The sections listed in `schema`, plus "Other" when `doc` has keys none of them lists. */
export function sectionsOf(schema: SiteConfigSchema, doc: Record<string, unknown> | null): ConfigSection[] {
	const sections = schema['x-sections'] ?? [];
	const known = new Set(sections.flatMap((s) => s.keys));
	const unknown = Object.keys(doc ?? {}).filter((k) => !known.has(k));
	return unknown.length ? [...sections, { ...OTHER_SECTION, keys: unknown }] : sections;
}

/** The section header line written by the server, e.g. `// ── Search & indexing ──`. */
export function sectionHeader(section: ConfigSection): string {
	return `// ── ${section.title} ──`;
}

/**
 * Removes `//` and `/* *\/` comments and trailing commas from JSONC, leaving strings intact.
 */
export function stripJsonc(text: string): string {
	let out = '';
	let i = 0;
	while (i < text.length) {
		const c = text[i];
		if (c === '"') {
			let j = i + 1;
			while (j < text.length && text[j] !== '"') j += text[j] === '\\' ? 2 : 1;
			out += text.slice(i, j + 1);
			i = j + 1;
		} else if (c === '/' && text[i + 1] === '/') {
			while (i < text.length && text[i] !== '\n') i++;
		} else if (c === '/' && text[i + 1] === '*') {
			const end = text.indexOf('*/', i + 2);
			i = end < 0 ? text.length : end + 2;
		} else {
			out += c;
			i++;
		}
	}
	return dropTrailingCommas(out);
}

/** Removes commas followed only by whitespace and a closing bracket, outside strings. */
function dropTrailingCommas(text: string): string {
	let out = '';
	for (let i = 0; i < text.length; i++) {
		const c = text[i];
		if (c === '"') {
			let j = i + 1;
			while (j < text.length && text[j] !== '"') j += text[j] === '\\' ? 2 : 1;
			out += text.slice(i, j + 1);
			i = j;
		} else if (c === ',' && /^\s*[}\]]/.test(text.slice(i + 1))) {
			continue;
		} else {
			out += c;
		}
	}
	return out;
}

/** Parses a JSONC object; null when the text is not valid JSONC or not an object. */
export function parseJsoncObject(text: string): Record<string, unknown> | null {
	try {
		const stripped = stripJsonc(text).trim();
		const value: unknown = stripped === '' ? {} : JSON.parse(stripped);
		return value && typeof value === 'object' && !Array.isArray(value)
			? (value as Record<string, unknown>)
			: null;
	} catch {
		return null;
	}
}

function escapeRegExp(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * The 1-based line where `section` starts in `text`: its header, or else the first line naming
 * one of its keys (set or commented out). Null when the document has neither.
 */
export function sectionLine(text: string, section: ConfigSection): number | null {
	const lines = text.split('\n');
	const header = sectionHeader(section);
	const h = lines.findIndex((l) => l.trim() === header);
	if (h >= 0) return h + 1;
	if (section.keys.length === 0) return null;
	const keyRe = new RegExp(
		`^\\s*(//\\s*)?"(${section.keys.map(escapeRegExp).join('|')})"\\s*:`
	);
	const k = lines.findIndex((l) => keyRe.test(l));
	return k >= 0 ? k + 1 : null;
}

/** The first sentence of a description, on one line. */
export function firstSentence(description: string | undefined): string {
	const flat = ((description ?? '').split('\n\n')[0] ?? '').replace(/\s+/g, ' ').trim();
	for (const m of flat.matchAll(/[.!?](?=\s|$)/g)) {
		const end = m.index + 1;
		// "e.g." and "i.e." do not end a sentence.
		if (!/\b(e\.g|i\.e)\.$/i.test(flat.slice(0, end))) return flat.slice(0, end);
	}
	return flat;
}

function compact(value: unknown): string {
	return JSON.stringify(value) ?? 'null';
}

/** The keys of `section` with their values in `doc` and their defaults. */
export function sectionRows(
	schema: SiteConfigSchema,
	section: ConfigSection,
	doc: Record<string, unknown> | null
): SectionRow[] {
	return section.keys.map((key) => {
		const prop = schema.properties?.[key];
		const set = doc !== null && Object.hasOwn(doc, key);
		const value = set ? compact(doc[key]) : null;
		const def = prop && 'default' in prop ? compact(prop.default) : null;
		return {
			key,
			description: firstSentence(prop?.description),
			set,
			value,
			default: def,
			isDefault: set && value === def
		};
	});
}

/** How many keys of `section` the document sets. */
export function setCount(section: ConfigSection, doc: Record<string, unknown> | null): number {
	return doc ? section.keys.filter((k) => Object.hasOwn(doc, k)).length : 0;
}

/** A key found by `searchKeys`. */
export interface KeyMatch {
	key: string;
	section: ConfigSection;
	/** First sentence of the key's description. */
	description: string;
}

/**
 * The keys whose name, description or section title contain every word of `query` (ignoring
 * case), keys whose name matches first; at most `limit`.
 */
export function searchKeys(schema: SiteConfigSchema, sections: ConfigSection[], query: string, limit = 20): KeyMatch[] {
	const words = query.toLowerCase().split(/\s+/).filter(Boolean);
	if (words.length === 0) return [];
	const byName: KeyMatch[] = [];
	const byText: KeyMatch[] = [];
	for (const section of sections) {
		for (const key of section.keys) {
			const description = schema.properties?.[key]?.description ?? '';
			const name = key.toLowerCase();
			const text = `${name} ${description} ${section.title}`.toLowerCase();
			if (!words.every((w) => text.includes(w))) continue;
			const match = { key, section, description: firstSentence(description) };
			(words.every((w) => name.includes(w)) ? byName : byText).push(match);
		}
	}
	return [...byName, ...byText].slice(0, limit);
}
