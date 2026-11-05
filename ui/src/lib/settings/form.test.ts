import { describe, expect, it } from 'vitest';
import { settingsForm, type SettingsSchema } from './form.ts';

const schema: SettingsSchema = {
	properties: {
		'search.defaultPatternType': { type: 'string', default: 'keyword' },
		'search.includeForks': { type: 'boolean', default: false },
		quicklinks: { type: 'array', default: [], items: { type: 'object', properties: { name: { type: 'string' } } } },
		'orgs.allMembersBatchChangesAdmin': { type: 'boolean', default: false }
	},
	'x-sections': [
		{ id: 'search', title: 'Search', description: '', keys: ['search.defaultPatternType', 'search.includeForks'] },
		{ id: 'home', title: 'Home', description: '', keys: ['quicklinks'] },
		{ id: 'organization', title: 'Organization', description: '', keys: ['orgs.allMembersBatchChangesAdmin'] }
	]
};

const keys = (sections: ReturnType<typeof settingsForm>) => sections.flatMap((s) => s.fields.map((f) => f.key));

describe('settingsForm', () => {
	it('leaves organization settings out of the global form, and the section with them', () => {
		const global = settingsForm(schema, {}, 'global');
		expect(global.map((s) => s.section.id)).toEqual(['search', 'home']);
		expect(keys(settingsForm(schema, {}, 'organization'))).toContain('orgs.allMembersBatchChangesAdmin');
	});

	it('leaves notices out of a user form, unless the document sets them', () => {
		const withNotices: SettingsSchema = {
			...schema,
			properties: { ...schema.properties, notices: { type: 'array', default: [] } },
			'x-sections': [{ id: 'home', title: 'Home', description: '', keys: ['notices'] }]
		};
		expect(keys(settingsForm(withNotices, {}, 'user'))).toEqual([]);
		expect(keys(settingsForm(withNotices, { notices: [] }, 'user'))).toEqual(['notices']);
		expect(keys(settingsForm(withNotices, {}, 'organization'))).toEqual(['notices']);
	});

	it('writes the notice form in plain words, message first', () => {
		const withNotices: SettingsSchema = {
			properties: {
				notices: {
					type: 'array',
					items: {
						type: 'object',
						required: ['location', 'message'],
						properties: {
							dismissible: { type: 'boolean' },
							location: {
								oneOf: [
									{ enum: ['top'], type: 'string' },
									{ enum: ['home'], type: 'string' }
								]
							},
							message: { type: 'string' }
						}
					}
				}
			},
			'x-sections': [{ id: 'home', title: 'Home', description: '', keys: ['notices'] }]
		};
		const items = settingsForm(withNotices, {}, 'global')[0]?.fields[0]?.itemFields ?? [];
		expect(items.map((f) => f.key)).toEqual(['message', 'location', 'dismissible']);
		expect(items[0]?.markdown).toBe(true);
		expect(items[1]?.options?.[0]).toMatchObject({ value: 'top', label: 'At the top of every page' });
	});

	it('shows a setting without effect only while the document sets it', () => {
		expect(keys(settingsForm(schema, {}, 'global'))).not.toContain('search.includeForks');
		expect(keys(settingsForm(schema, { 'search.includeForks': true }, 'global'))).toContain('search.includeForks');
	});

	it('uses the inherited value as the default', () => {
		const [search] = settingsForm(schema, {}, 'organization', { 'search.defaultPatternType': 'regexp' });
		expect(search?.fields[0]?.default).toBe('regexp');
		const [plain] = settingsForm(schema, {}, 'organization');
		expect(plain?.fields[0]?.default).toBe('keyword');
	});

	it('lists keys the schema does not know', () => {
		const sections = settingsForm(schema, { 'my.flag': 1 }, 'global');
		expect(sections.at(-1)?.section.id).toBe('other');
		expect(sections.at(-1)?.fields[0]?.kind).toBe('json');
	});
});
