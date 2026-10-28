import { describe, expect, it } from 'vitest';
import { describeEventTypes, eventTypesInput, validateWebhookForm } from './outbound-webhooks.ts';

describe('outbound webhook helpers', () => {
	it('builds event type input', () => {
		expect(
			eventTypesInput(
				{
					'changeset:publish': true,
					'batch_change:apply': true,
					'changeset:close': false
				},
				{
					'changeset:publish': ' repo-1 '
				}
			)
		).toEqual([
			{ eventType: 'batch_change:apply', scope: null },
			{ eventType: 'changeset:publish', scope: 'repo-1' }
		]);
	});

	it('validates the form', () => {
		const t = [{ eventType: 'x', scope: null }];
		expect(validateWebhookForm('https://hooks.example.com/', 's', true, t)).toEqual([]);
		expect(validateWebhookForm('ftp://x', 's', true, t)).toEqual([
			'The URL must be http or https.'
		]);
		expect(validateWebhookForm('nope', '', true, [])).toHaveLength(3);
		expect(validateWebhookForm('http://x', '', false, t)).toEqual([]);
		// Editing, an empty URL keeps the stored one; creating needs one.
		expect(validateWebhookForm('', '', false, t)).toEqual([]);
		expect(validateWebhookForm('', 's', true, t)).toEqual(['Enter a valid URL.']);
	});

	it('describes event types', () => {
		expect(
			describeEventTypes([
				{ eventType: 'a', scope: null },
				{ eventType: 'b', scope: 's' }
			])
		).toBe('a, b (s)');
	});
});
