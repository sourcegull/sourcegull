import { describe, expect, it } from 'vitest';
import { GraphQLRequestError } from '#lib/graphql.ts';
import {
	duration,
	isActive,
	isCancelable,
	reasonLabel,
	setFlash,
	takeFlash,
	validationMessages
} from './queries.ts';

describe('code host connection helpers', () => {
	it('formats sync durations', () => {
		expect(duration('2026-01-01T00:00:00Z', '2026-01-01T00:00:09.6Z')).toBe('10s');
		expect(duration('2026-01-01T00:00:00Z', '2026-01-01T00:01:05Z')).toBe('1m 05s');
		expect(duration('2026-01-01T00:00:00Z', '2026-01-01T02:03:00Z')).toBe('2h 03m');
		expect(duration('2026-01-01T00:00:00Z', null)).toBeNull();
		expect(duration('2026-01-01T00:01:00Z', '2026-01-01T00:00:00Z')).toBeNull();
	});

	it('knows which sync jobs can be canceled and which are active', () => {
		expect(isCancelable('QUEUED')).toBe(true);
		expect(isCancelable('PROCESSING')).toBe(true);
		expect(isCancelable('CANCELING')).toBe(false);
		expect(isCancelable('COMPLETED')).toBe(false);
		expect(isActive('CANCELING')).toBe(true);
		expect(isActive('FAILED')).toBe(false);
	});

	it('labels sync reasons', () => {
		expect(reasonLabel('manual')).toBe('Sync now');
		expect(reasonLabel('config_changed')).toBe('Configuration changed');
		expect(reasonLabel('something_new')).toBe('something_new');
	});

	it('extracts the schema problems of VALIDATION errors', () => {
		const err = new GraphQLRequestError('The configuration is invalid', 200, [
			{
				message: 'The configuration is invalid',
				extensions: { code: 'VALIDATION', messages: ['/url: is required'] }
			}
		]);
		expect(validationMessages(err)).toEqual(['/url: is required']);
		const other = new GraphQLRequestError('nope', 200, [
			{ message: 'nope', extensions: { code: 'FORBIDDEN' } }
		]);
		expect(validationMessages(other)).toBeNull();
		expect(validationMessages(new Error('x'))).toBeNull();
	});

	it('hands a flash notice over once', () => {
		setFlash('warning');
		expect(takeFlash()).toBe('warning');
		expect(takeFlash()).toBeNull();
	});
});
