/**
 * The forms view of a settings document: the sections and fields of the settings JSON Schema
 * that apply to a subject, with the value the subject inherits as each field's default.
 */
import { sectionsOf, type ConfigSection, type SiteConfigSchema } from '#lib/site-config-sections.ts';
import { sectionFields, type FormField, type JsonSchema } from '#lib/site-config-form.ts';

/** The subject whose settings a form edits. */
export type SettingsScope = 'global' | 'organization' | 'user';

export type SettingsSchema = SiteConfigSchema & JsonSchema;

export interface SettingsFormSection {
	section: ConfigSection;
	fields: FormField[];
}

/** Settings without any effect, shown only when a document still sets them. */
const NO_EFFECT = new Set(['search.includeForks']);
/** Settings read only from an organization's own document. */
const ORGANIZATION_ONLY = new Set(['orgs.allMembersBatchChangesAdmin']);
/** Settings for others to read: in a user's own settings they would only hide what others set. */
const NOT_FOR_USERS = new Set(['notices']);
/** Labels the key names don't give, in the words the UI uses. */
const TITLES: Record<string, string> = {
	'orgs.allMembersBatchChangesAdmin': 'All members administer bulk changes'
};

/** The notice form: what is written first, and the location and dismissal in plain words. */
function refineNotices(field: FormField): FormField {
	const copy: Record<string, Partial<FormField>> = {
		message: {
			markdown: true,
			description: 'The text of the banner. Markdown works, for example `**bold**` and `[a link](https://example.com)`.'
		},
		location: { title: 'Where it appears', description: '' },
		dismissible: { title: 'Users can dismiss it', description: '' }
	};
	const labels: Record<string, string> = { top: 'At the top of every page', home: 'On the search home page' };
	const order = ['message', 'location', 'dismissible'];
	return {
		...field,
		itemFields: (field.itemFields ?? [])
			.map((f) => ({
				...f,
				...copy[f.key],
				options: f.options?.map((o) => ({ ...o, label: labels[o.value], description: '' }))
			}))
			.sort((a, b) => order.indexOf(a.key) - order.indexOf(b.key))
	};
}

function applies(key: string, scope: SettingsScope, doc: Record<string, unknown>): boolean {
	if (key in doc) return true;
	if (NO_EFFECT.has(key)) return false;
	if (scope === 'user' && NOT_FOR_USERS.has(key)) return false;
	return scope === 'organization' || !ORGANIZATION_ONLY.has(key);
}

/**
 * The sections to show for `doc`. A value in `inherited` (the higher level of the cascade)
 * replaces the schema's default, since that is what applies when the document has no value.
 */
export function settingsForm(
	schema: SettingsSchema,
	doc: Record<string, unknown>,
	scope: SettingsScope,
	inherited: Record<string, unknown> | null = null
): SettingsFormSection[] {
	return sectionsOf(schema, doc)
		.map((section) => ({
			section,
			fields: sectionFields(
				schema,
				section.keys.filter((key) => applies(key, scope, doc))
			).map((field) => ({
				...(field.key === 'notices' ? refineNotices(field) : field),
				title: TITLES[field.key] ?? field.title,
				default: inherited?.[field.key] ?? field.default
			}))
		}))
		.filter((s) => s.fields.length > 0);
}
