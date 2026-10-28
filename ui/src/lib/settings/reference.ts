/**
 * The settings reference: one row per settings key, read from the settings JSON Schema the server
 * serves at `/.api/settings-schema` (generated from the typed settings in `crates/config`).
 */

/** Where the server serves the settings JSON Schema. */
export const SETTINGS_SCHEMA_URL = '/.api/settings-schema';
/** The settings reference page. */
export const SETTINGS_REFERENCE_PATH = '/help/settings';

/** A JSON Schema node (the subset schemars emits). */
export interface SchemaNode {
	type?: string | string[];
	description?: string;
	default?: unknown;
	enum?: unknown[];
	oneOf?: SchemaNode[];
	anyOf?: SchemaNode[];
	allOf?: SchemaNode[];
	$ref?: string;
	items?: SchemaNode;
	properties?: Record<string, SchemaNode>;
	required?: string[];
	additionalProperties?: boolean | SchemaNode;
	definitions?: Record<string, SchemaNode>;
	$defs?: Record<string, SchemaNode>;
}

/** An allowed value of an enumerated setting. */
export interface SettingValue {
	value: string;
	description: string | null;
}

/** One documented setting. */
export interface SettingRow {
	key: string;
	type: string;
	description: string;
	/** The default as JSON text, or null when there is none. */
	default: string | null;
	/** Allowed values, for enumerations. */
	values: SettingValue[];
	/** Fields of each array item or object value, for structured settings. */
	fields: SettingRow[];
}

function resolve(root: SchemaNode, node: SchemaNode): SchemaNode {
	let n = node;
	// `allOf: [{$ref}]` wraps a reference to attach a description and default.
	const wrapped = n.allOf?.length === 1 ? n.allOf[0] : undefined;
	if (!n.$ref && wrapped?.$ref) n = { ...wrapped, ...n, allOf: undefined };
	for (let depth = 0; n.$ref && depth < 10; depth++) {
		const m = /^#\/(definitions|\$defs)\/(.+)$/.exec(n.$ref);
		const defs = m?.[1] === 'definitions' ? root.definitions : root.$defs;
		const target = m?.[2] !== undefined ? defs?.[m[2]] : undefined;
		if (!target) break;
		const { $ref: _ref, ...rest } = n;
		n = { ...target, ...rest };
	}
	return n;
}

function enumValues(node: SchemaNode): SettingValue[] {
	if (node.enum) return node.enum.map((v) => ({ value: String(v), description: null }));
	const variants = node.oneOf ?? node.anyOf ?? [];
	return variants.flatMap((v) =>
		(v.enum ?? []).map((value) => ({ value: String(value), description: v.description ?? null }))
	);
}

function typeName(root: SchemaNode, node: SchemaNode): string {
	if (enumValues(node).length) return 'enum';
	const t = Array.isArray(node.type) ? node.type.join(' | ') : node.type;
	if (t === 'array' && node.items) return `array of ${typeName(root, resolve(root, node.items))}`;
	return t ?? 'any';
}

function rows(root: SchemaNode, node: SchemaNode): SettingRow[] {
	return Object.entries(node.properties ?? {})
		.map(([key, raw]) => {
			const n = resolve(root, raw);
			const item = n.type === 'array' && n.items ? resolve(root, n.items) : n;
			return {
				key,
				type: typeName(root, n),
				description: raw.description ?? n.description ?? '',
				default: n.default === undefined ? null : JSON.stringify(n.default),
				values: enumValues(n),
				fields: item.properties ? rows(root, item) : []
			};
		})
		.sort((a, b) => a.key.localeCompare(b.key));
}

/** The documented settings keys of `schema`, sorted by key. */
export function settingsReference(schema: SchemaNode): SettingRow[] {
	return rows(schema, schema);
}
