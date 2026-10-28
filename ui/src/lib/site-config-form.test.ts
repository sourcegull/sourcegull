import { describe, expect, it } from 'vitest';
import {
	buildField,
	getAt,
	humanize,
	isSecret,
	issuesAt,
	issuesFromError,
	keyTitle,
	parseIssue,
	resolveSchema,
	sectionFields,
	type FormField,
	type JsonSchema
} from './site-config-form.ts';

/** A schema shaped like the one schemars generates for the site config. */
const schema: JsonSchema = {
	definitions: {
		AccessTokens: {
			type: 'object',
			properties: {
				allow: { allOf: [{ $ref: '#/definitions/AccessTokenAllow' }], description: 'Who may create access tokens.' },
				maxTokenExpiresInDays: { type: 'integer', minimum: 1 }
			}
		},
		AccessTokenAllow: {
			oneOf: [
				{ enum: ['all'], type: 'string', description: 'Any user.' },
				{ enum: ['none'], type: 'string', description: 'Disabled.' }
			]
		},
		Duration: { type: 'string', pattern: '^([0-9]+(ms|s|m|h|d|w))+$' },
		SmtpConfig: {
			type: 'object',
			required: ['host', 'port'],
			properties: {
				host: { type: 'string' },
				password: { type: ['string', 'null'], 'x-secret': true },
				port: { type: 'integer', format: 'uint16', minimum: 0 }
			}
		},
		Rule: { type: 'object', properties: { pattern: { type: 'string' } } }
	},
	properties: {
		externalURL: { type: ['string', 'null'], description: 'The URL.' },
		'auth.public': { type: 'boolean', default: false },
		'auth.sessionExpiry': { allOf: [{ $ref: '#/definitions/Duration' }], default: '30d' },
		'auth.accessTokens': {
			allOf: [{ $ref: '#/definitions/AccessTokens' }],
			default: { allow: 'all', maxTokenExpiresInDays: 365 }
		},
		'email.smtp': { anyOf: [{ $ref: '#/definitions/SmtpConfig' }, { type: 'null' }] },
		'webhooks.outbound.allowedHosts': { type: 'array', items: { type: 'string' } },
		gitUpdateInterval: { type: 'array', items: { $ref: '#/definitions/Rule' } },
		'codeIntelAutoIndexing.indexerMap': { type: 'object', additionalProperties: { type: 'string' } },
		'batchChanges.commitViaAPIMaxBytes': { type: 'integer', minimum: 1, maximum: 50331648, default: 1024 },
		'scim.authToken': { type: ['string', 'null'], minLength: 20, 'x-secret': true }
	}
};

describe('resolveSchema', () => {
	it('follows refs, allOf wrappers and nullable unions', () => {
		const smtp = resolveSchema(schema, schema.properties!['email.smtp']!);
		expect(smtp.type).toBe('object');
		expect(smtp.nullable).toBe(true);
		const expiry = resolveSchema(schema, schema.properties!['auth.sessionExpiry']!);
		expect(expiry.type).toBe('string');
		expect(expiry.default).toBe('30d');
		const url = resolveSchema(schema, schema.properties!.externalURL!);
		expect(url.type).toBe('string');
		expect(url.nullable).toBe(true);
	});
});

describe('schema to form model', () => {
	const fields = sectionFields(schema, Object.keys(schema.properties!));
	const at = (key: string): FormField => {
		const f = fields.find((x) => x.key === key);
		if (!f) throw new Error(`no field ${key}`);
		return f;
	};

	it('picks a control per type', () => {
		expect(at('auth.public').kind).toBe('boolean');
		expect(at('auth.public').default).toBe(false);
		expect(at('externalURL').kind).toBe('string');
		expect(at('auth.sessionExpiry').kind).toBe('string');
		expect(at('auth.sessionExpiry').pattern).toBeDefined();
		expect(at('webhooks.outbound.allowedHosts').kind).toBe('stringList');
		expect(at('gitUpdateInterval').kind).toBe('json');
		expect(at('codeIntelAutoIndexing.indexerMap').kind).toBe('json');
	});

	it('keeps numeric bounds', () => {
		const f = at('batchChanges.commitViaAPIMaxBytes');
		expect(f.kind).toBe('integer');
		expect([f.min, f.max, f.default]).toEqual([1, 50331648, 1024]);
	});

	it('nests simple objects with defaults from the parent default', () => {
		const f = at('auth.accessTokens');
		expect(f.kind).toBe('object');
		const allow = f.children!.find((c) => c.key === 'allow')!;
		expect(allow.kind).toBe('enum');
		expect(allow.options).toEqual([
			{ value: 'all', description: 'Any user.' },
			{ value: 'none', description: 'Disabled.' }
		]);
		expect(allow.default).toBe('all');
		expect(allow.path).toEqual(['auth.accessTokens', 'allow']);
		const smtp = at('email.smtp');
		expect(smtp.kind).toBe('object');
		expect(smtp.nullable).toBe(true);
		expect(smtp.children!.find((c) => c.key === 'host')!.required).toBe(true);
	});

	it('marks secrets from x-secret and by name', () => {
		expect(at('scim.authToken').secret).toBe(true);
		expect(at('email.smtp').children!.find((c) => c.key === 'password')!.secret).toBe(true);
		expect(at('externalURL').secret).toBe(false);
		expect(isSecret('clientSecret', { type: 'string' })).toBe(true);
		expect(isSecret('executors.accessToken', { type: 'string' })).toBe(true);
		expect(isSecret('allowSignup', { type: 'boolean' })).toBe(false);
		expect(isSecret('maxTokenExpiresInDays', { type: 'integer' })).toBe(false);
	});

	it('titles keys readably', () => {
		expect(keyTitle('externalURL')).toBe('External URL');
		expect(keyTitle('auth.sessionExpiry')).toBe('Session expiry');
		expect(keyTitle('batchChanges.enabled')).toBe('Batch changes enabled');
		expect(keyTitle('insights.query.worker.rateLimit')).toBe('Query worker rate limit');
		expect(humanize('clientID')).toBe('Client ID');
		expect(humanize('noVerifyTLS')).toBe('No verify TLS');
		expect(buildField(schema, 'url', { type: 'string' }, ['auth.providers', 0, 'url']).title).toBe('URL');
	});
});

describe('values and issues', () => {
	it('reads values by path', () => {
		const doc = { 'auth.providers': [{ type: 'gitlab', clientID: 'x' }] };
		expect(getAt(doc, ['auth.providers', 0, 'clientID'])).toBe('x');
		expect(getAt(doc, ['auth.providers', 3, 'clientID'])).toBeUndefined();
		expect(getAt(doc, ['missing', 'deep'])).toBeUndefined();
	});

	it('parses server pointers, unescaping them', () => {
		expect(parseIssue('/auth.providers/1: "clientID" is a required property')).toEqual({
			path: ['auth.providers', 1],
			message: '"clientID" is a required property'
		});
		expect(parseIssue('/a~1b: bad').path).toEqual(['a/b']);
		expect(parseIssue('/: root problem')).toEqual({ path: [], message: 'root problem' });
		expect(parseIssue('syntax error: x').path).toEqual([]);
	});

	it('maps save errors onto fields', () => {
		const issues = issuesFromError(
			'The site configuration is invalid:\n- /auth.public: "yes" is not of type "boolean"\n- /auth.providers/0/clientID: too short'
		);
		expect(issues).toHaveLength(2);
		expect(issuesAt(issues, ['auth.public'])).toHaveLength(1);
		expect(issuesAt(issues, ['auth.providers', 0, 'clientID'])).toHaveLength(1);
		// An object fieldset shows only problems at its own path.
		expect(issuesAt(issues, ['auth.providers'], false)).toHaveLength(0);
		expect(issuesAt(issues, ['auth.providers'], true)).toHaveLength(1);
	});
});
