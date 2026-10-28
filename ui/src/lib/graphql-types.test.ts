/**
 * Generates `graphql-types.ts`, the result and variable types of every GraphQL operation the UI
 * sends, from `ui/schema.graphql` and the exported operation strings in `src/lib`.
 *
 * `npm test` fails when the checked-in file is not current; `npm run graphql-types` rewrites it.
 */
import { readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { codegen } from '@graphql-codegen/core';
import * as typescriptOperations from '@graphql-codegen/typescript-operations';
import { expect, it } from 'vitest';

// The codegen packages load graphql's CommonJS build; its schema and documents must come from
// the same instance.
const { buildSchema, parse } = createRequire(import.meta.url)('graphql') as typeof import('graphql');

const SCHEMA_FILE = new URL('../../schema.graphql', import.meta.url);
const OUTPUT_FILE = new URL('./graphql-types.ts', import.meta.url);

const HEADER = `/**
 * Result and variable types of the UI's GraphQL operations, generated from ui/schema.graphql by
 * src/lib/graphql-types.test.ts. Do not edit; run \`npm run graphql-types\`.
 */
/* eslint-disable */
`;

const modules = import.meta.glob(['./**/*.ts', '!./**/*.test.ts', '!./graphql-types.ts'], {
	eager: true
}) as Record<string, Record<string, unknown>>;

/** Exported operation strings, in a stable order (by file, then export name). */
function operations(): { location: string; document: ReturnType<typeof parse> }[] {
	return Object.entries(modules)
		.sort(([a], [b]) => a.localeCompare(b))
		.flatMap(([file, mod]) =>
			Object.entries(mod)
				.filter(([, v]) => typeof v === 'string' && /^\s*(query|mutation)\b/.test(v))
				.sort(([a], [b]) => a.localeCompare(b))
				.map(([name, v]) => ({ location: `${file}:${name}`, document: parse(v as string) }))
		);
}

async function generate(): Promise<string> {
	const schemaText = readFileSync(SCHEMA_FILE, 'utf8');
	const output = await codegen({
		filename: 'graphql-types.ts',
		schema: parse(schemaText),
		schemaAst: buildSchema(schemaText),
		documents: operations(),
		config: {
			enumsAsTypes: true,
			onlyOperationTypes: true,
			skipTypename: true,
			avoidOptionals: { field: true, inputValue: false, object: true, defaultValue: false },
			useTypeImports: true,
			scalars: {
				BigInt: 'string',
				DateTime: 'string',
				JSONValue: 'unknown',
				PublishedValue: "boolean | 'draft'"
			}
		},
		plugins: [{ typescriptOperations: {} }],
		pluginMap: { typescriptOperations }
	});
	return HEADER + output.trimEnd() + '\n';
}

it('graphql-types.ts is current', async () => {
	const generated = await generate();
	if (process.env['GRAPHQL_TYPES_WRITE'] === '1') {
		writeFileSync(OUTPUT_FILE, generated);
		return;
	}
	let current = '';
	try {
		current = readFileSync(OUTPUT_FILE, 'utf8');
	} catch {
		// Missing: reported by the comparison below.
	}
	expect(current === generated, 'src/lib/graphql-types.ts is stale: run `npm run graphql-types`').toBe(true);
}, 60_000);
