/**
 * The forms view of the site configuration: turns the site config JSON Schema (generated in
 * `crates/config`, draft 7 as schemars writes it) into a list of form fields per section, and
 * maps server validation messages (`/pointer: message`) onto those fields.
 */

/** The subset of JSON Schema the forms read. */
export interface JsonSchema {
	type?: string | string[];
	title?: string;
	description?: string;
	default?: unknown;
	enum?: unknown[];
	const?: unknown;
	oneOf?: JsonSchema[];
	anyOf?: JsonSchema[];
	allOf?: JsonSchema[];
	$ref?: string;
	properties?: Record<string, JsonSchema>;
	required?: string[];
	items?: JsonSchema;
	additionalProperties?: boolean | JsonSchema;
	minimum?: number;
	maximum?: number;
	minLength?: number;
	pattern?: string;
	format?: string;
	definitions?: Record<string, JsonSchema>;
	'x-secret'?: boolean;
	'x-section'?: string;
	writeOnly?: boolean;
}

/** A path into the configuration document: object keys and array indexes. */
export type ConfigPath = (string | number)[];

export type FieldKind = 'boolean' | 'string' | 'integer' | 'number' | 'enum' | 'stringList' | 'object' | 'json';

export interface EnumOption {
	value: string;
	description: string;
}

/** One form control (or fieldset, for `object`). */
export interface FormField {
	/** The property name, e.g. `auth.sessionExpiry` or `allowSignup`. */
	key: string;
	path: ConfigPath;
	title: string;
	description: string;
	kind: FieldKind;
	/** The default the server applies when the key is absent (undefined when none). */
	default: unknown;
	nullable: boolean;
	secret: boolean;
	required: boolean;
	min?: number;
	max?: number;
	minLength?: number;
	pattern?: string;
	options?: EnumOption[];
	/** The fields of an `object`. */
	children?: FormField[];
}

const DEFINITIONS = '#/definitions/';

/**
 * Follows `$ref`, single-element `allOf` wrappers and nullable `anyOf: [X, null]` unions, and
 * merges the outer node's description and default over the target's. `nullable` is set when
 * `null` is allowed.
 */
export function resolveSchema(root: JsonSchema, node: JsonSchema): JsonSchema & { nullable?: boolean } {
	let out: JsonSchema & { nullable?: boolean } = { ...node };
	for (let guard = 0; guard < 16; guard++) {
		if (out.$ref?.startsWith(DEFINITIONS)) {
			const target = root.definitions?.[out.$ref.slice(DEFINITIONS.length)] ?? {};
			const { $ref: _, ...rest } = out;
			out = { ...target, ...rest };
		} else if (out.allOf?.length === 1 && !out.type && !out.properties) {
			const { allOf, ...rest } = out;
			out = { ...allOf[0], ...rest };
		} else if (out.anyOf?.length === 2 && out.anyOf.some(isNull)) {
			const { anyOf, ...rest } = out;
			out = { ...anyOf.find((s) => !isNull(s)), ...rest, nullable: true };
		} else break;
	}
	if (Array.isArray(out.type)) {
		const types = out.type.filter((t) => t !== 'null');
		if (types.length < out.type.length) out.nullable = true;
		out.type = types.length === 1 ? types[0] : types;
	}
	return out;
}

function isNull(s: JsonSchema): boolean {
	return s.type === 'null';
}

/** Name fragments that mark a credential when the schema has no `x-secret`. */
const SECRET_NAME = /(secret|password|token|privatekey|apikey)$/i;

/** Whether the property `key` holds a credential: `x-secret`/`writeOnly` or a telling name. */
export function isSecret(key: string, schema: JsonSchema): boolean {
	if (schema['x-secret'] || schema.writeOnly) return true;
	const last = key.split('.').pop() ?? key;
	return SECRET_NAME.test(last) && !/^(allow|max|enable|enforce)/i.test(last);
}

/** Enum options of `schema`: from `enum`, or from a `oneOf` of single-value enums with docs. */
export function enumOptions(schema: JsonSchema): EnumOption[] | null {
	if (schema.enum?.every((v) => typeof v === 'string')) {
		return (schema.enum as string[]).map((value) => ({ value, description: '' }));
	}
	if (schema.oneOf?.length && schema.oneOf.every((o) => o.enum?.length === 1 && typeof o.enum[0] === 'string')) {
		return schema.oneOf.map((o) => ({ value: o.enum![0] as string, description: o.description ?? '' }));
	}
	return null;
}

const ACRONYMS = new Set(['url', 'smtp', 'id', 'tls', 'api', 'ip', 'scim', 'oidc', 'otlp', 'scip', 'http', 'cidr']);

/** Splits a camelCase or dotted identifier into words: `externalURL` → `External URL`. */
export function humanize(id: string): string {
	const words = id
		.replace(/[._-]+/g, ' ')
		.replace(/([a-z0-9])([A-Z])/g, '$1 $2')
		.replace(/([A-Z]+)([A-Z][a-z])/g, '$1 $2')
		.trim()
		.split(/\s+/)
		.map((w, i) =>
			/^[A-Z0-9]{2,}$/.test(w)
				? w
				: ACRONYMS.has(w.toLowerCase())
					? w.toUpperCase()
					: i === 0
						? w.charAt(0).toUpperCase() + w.slice(1)
						: w.toLowerCase()
		);
	return words.join(' ');
}

/**
 * The label of a top-level key. The first dotted segment usually names the section and is
 * dropped (`auth.sessionExpiry` → `Session expiry`), unless the rest is one generic word
 * (`batchChanges.enabled` → `Bulk changes enabled`).
 */
export function keyTitle(key: string): string {
	const parts = key.split('.');
	if (parts.length === 1) return humanize(key);
	const last = parts[parts.length - 1] ?? '';
	const rest = parts.slice(1).join('.');
	if (/^[a-z]+$/.test(last) && parts.length >= 2) {
		return humanize(parts.slice(0, -1).join(' ')) + ' ' + last;
	}
	return humanize(rest);
}

/** Builds the field for property `key` (schema `node`) at `path`. */
export function buildField(
	root: JsonSchema,
	key: string,
	node: JsonSchema,
	path: ConfigPath,
	options: { required?: boolean; title?: string; depth?: number } = {}
): FormField {
	const s = resolveSchema(root, node);
	const field: FormField = {
		key,
		path,
		title: options.title ?? (path.length === 1 ? keyTitle(key) : humanize(key)),
		description: s.description ?? '',
		kind: 'json',
		default: s.default,
		nullable: !!s.nullable,
		secret: isSecret(key, s),
		required: !!options.required
	};
	const opts = enumOptions(s);
	if (opts) {
		field.kind = 'enum';
		field.options = opts;
	} else if (s.type === 'boolean') {
		field.kind = 'boolean';
	} else if (s.type === 'string') {
		field.kind = 'string';
		field.minLength = s.minLength;
		field.pattern = s.pattern;
	} else if (s.type === 'integer' || s.type === 'number') {
		field.kind = s.type;
		field.min = s.minimum;
		field.max = s.maximum;
	} else if (s.type === 'array' && s.items) {
		const item = resolveSchema(root, s.items);
		if (item.type === 'string' && !enumOptions(item)) field.kind = 'stringList';
	} else if (s.type === 'object' && s.properties && (options.depth ?? 0) < 3) {
		const props = s.properties;
		const required = new Set(s.required ?? []);
		const defaults = (s.default && typeof s.default === 'object' ? s.default : {}) as Record<string, unknown>;
		field.kind = 'object';
		field.children = Object.keys(props).map((k) => {
			const child = buildField(root, k, props[k] ?? {}, [...path, k], {
				required: required.has(k),
				depth: (options.depth ?? 0) + 1
			});
			if (child.default === undefined && k in defaults) child.default = defaults[k];
			return child;
		});
	}
	return field;
}

/** The fields of the section listing `keys`, in that order (keys the schema lacks are JSON). */
export function sectionFields(schema: JsonSchema, keys: string[]): FormField[] {
	return keys.map((key) => buildField(schema, key, schema.properties?.[key] ?? {}, [key]));
}

/** The value at `path` in `doc`, or undefined. */
export function getAt(doc: unknown, path: ConfigPath): unknown {
	let cur: unknown = doc;
	for (const seg of path) {
		if (cur === null || typeof cur !== 'object') return undefined;
		cur = (cur as Record<string, unknown>)[seg as string];
	}
	return cur;
}

/** Deep equality of JSON values. */
export function jsonEqual(a: unknown, b: unknown): boolean {
	return JSON.stringify(a) === JSON.stringify(b);
}

/** One server validation problem, with the document path it points at. */
export interface ConfigIssue {
	path: ConfigPath;
	message: string;
}

/**
 * Parses a validation message as the server formats it (`/auth.providers/1: "clientID" is a
 * required property`, optionally after a `- ` bullet). Messages without a pointer get an empty
 * path.
 */
export function parseIssue(text: string): ConfigIssue {
	const m = /^\s*(?:-\s+)?(\/\S*):\s+([\s\S]*)$/.exec(text);
	if (!m) return { path: [], message: text.trim() };
	const pointer = m[1] === '/' ? '' : (m[1] ?? '');
	const path = pointer
		.split('/')
		.slice(1)
		.map((seg) => seg.replace(/~1/g, '/').replace(/~0/g, '~'))
		.map((seg) => (/^\d+$/.test(seg) ? Number(seg) : seg));
	return { path, message: (m[2] ?? '').trim() };
}

/** Issues from a save error (`The site configuration is invalid:\n- /x: …`), one per line. */
export function issuesFromError(text: string): ConfigIssue[] {
	return text
		.split('\n')
		.filter((line) => /^\s*-\s+\//.test(line))
		.map(parseIssue);
}

function samePath(a: ConfigPath, b: ConfigPath): boolean {
	return a.length === b.length && a.every((seg, i) => String(seg) === String(b[i]));
}

function startsWith(path: ConfigPath, prefix: ConfigPath): boolean {
	return prefix.length <= path.length && prefix.every((seg, i) => String(seg) === String(path[i]));
}

/**
 * The issues a field shows: those at exactly its path, and for leaf fields also those below
 * it (e.g. an item of a list).
 */
export function issuesAt(issues: ConfigIssue[], path: ConfigPath, leaf = true): ConfigIssue[] {
	return issues.filter((i) => (leaf ? startsWith(i.path, path) : samePath(i.path, path)));
}

/** Splits text on backticks so `code` spans can be rendered as code. */
export function codeSpans(text: string): { code: boolean; text: string }[] {
	return text
		.split('`')
		.map((t, i) => ({ code: i % 2 === 1, text: t }))
		.filter((p) => p.text !== '');
}

/** A short text for a value (used for defaults in placeholders and hints). */
export function displayValue(value: unknown): string {
	if (value === undefined || value === null) return '';
	if (typeof value === 'string') return value;
	return JSON.stringify(value);
}
