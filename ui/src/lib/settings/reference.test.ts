import { describe, expect, it } from 'vitest';
import { settingsReference, type SchemaNode } from './reference.ts';

// The shape schemars 0.8 emits for `crates/config/src/settings.rs`.
const schema: SchemaNode = {
	type: 'object',
	properties: {
		'search.includeForks': {
			description: 'Include forks.',
			default: false,
			type: 'boolean'
		},
		'search.defaultPatternType': {
			description: 'Pattern type used when a query has no `patternType:`.',
			default: 'keyword',
			allOf: [{ $ref: '#/definitions/PatternType' }]
		},
		quicklinks: {
			description: 'Links shown on the search home page.',
			default: [],
			type: 'array',
			items: { $ref: '#/definitions/Quicklink' }
		}
	},
	additionalProperties: true,
	definitions: {
		PatternType: {
			description: 'Search pattern type.',
			oneOf: [
				{ description: 'Terms are ANDed.', type: 'string', enum: ['keyword'] },
				{ description: 'Regular expression.', type: 'string', enum: ['regexp'] }
			]
		},
		Quicklink: {
			type: 'object',
			required: ['name', 'url'],
			properties: {
				name: { description: 'Link text.', type: 'string' },
				url: { description: 'Target URL.', type: 'string' }
			}
		}
	}
};

describe('settingsReference', () => {
	const ref = settingsReference(schema);
	const row = (key: string) => {
		const r = ref.find((x) => x.key === key);
		if (!r) throw new Error(`no row ${key}`);
		return r;
	};
	it('lists keys sorted with descriptions and defaults', () => {
		expect(ref.map((r) => r.key)).toEqual([
			'quicklinks',
			'search.defaultPatternType',
			'search.includeForks'
		]);
		const forks = row('search.includeForks');
		expect(forks).toMatchObject({ type: 'boolean', default: 'false', description: 'Include forks.' });
	});
	it('resolves enumerations through references', () => {
		const pt = row('search.defaultPatternType');
		expect(pt.type).toBe('enum');
		expect(pt.default).toBe('"keyword"');
		expect(pt.description).toBe('Pattern type used when a query has no `patternType:`.');
		expect(pt.values).toEqual([
			{ value: 'keyword', description: 'Terms are ANDed.' },
			{ value: 'regexp', description: 'Regular expression.' }
		]);
	});
	it('lists the fields of array items', () => {
		const q = row('quicklinks');
		expect(q.type).toBe('array of object');
		expect(q.default).toBe('[]');
		expect(q.fields.map((f) => [f.key, f.type])).toEqual([
			['name', 'string'],
			['url', 'string']
		]);
	});
});
