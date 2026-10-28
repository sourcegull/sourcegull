/**
 * Every GraphQL operation the UI sends is valid against the server's schema (`ui/schema.graphql`,
 * kept equal to the server's SDL by `crates/graphql/tests/schema_file.rs`).
 */
import { readFileSync } from 'node:fs';
import { buildSchema, parse, validate } from 'graphql';
import { describe, expect, it } from 'vitest';

const schema = buildSchema(readFileSync(new URL('../../schema.graphql', import.meta.url), 'utf8'));

const modules = import.meta.glob(['./**/*.ts', '!./**/*.test.ts'], { eager: true }) as Record<
	string,
	Record<string, unknown>
>;

const operations = Object.entries(modules).flatMap(([file, mod]) =>
	Object.entries(mod)
		.filter(([, v]) => typeof v === 'string' && /^\s*(query|mutation)\b/.test(v))
		.map(([name, v]) => [`${file}:${name}`, v as string] as const)
);

describe('GraphQL operations', () => {
	it('are found', () => {
		expect(operations.length).toBeGreaterThan(20);
	});
	it.each(operations)('%s is valid', (_name, op) => {
		const errors = validate(schema, parse(op)).map((e) => e.message);
		expect(errors).toEqual([]);
	});
});
