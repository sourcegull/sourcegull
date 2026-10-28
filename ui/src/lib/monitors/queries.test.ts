import { describe, expect, it } from 'vitest';
import { actionDraft, actionEditInput } from './queries.ts';

describe('stored secrets in the watch form', () => {
	it('starts the URL empty and sends it back empty to keep the stored one', () => {
		const draft = actionDraft({
			__typename: 'MonitorSlackWebhook',
			id: 'A1',
			enabled: true,
			includeResults: false,
			url: 'https://hooks.slack.com/…a9f3'
		});
		expect(draft.url).toBe('');
		expect(draft.masked).toBe('https://hooks.slack.com/…a9f3');
		expect(actionEditInput(draft)).toEqual({
			slackWebhook: { id: 'A1', update: { enabled: true, includeResults: false, url: '' } }
		});
		draft.url = ' https://hooks.slack.com/services/new ';
		expect(actionEditInput(draft)).toEqual({
			slackWebhook: {
				id: 'A1',
				update: { enabled: true, includeResults: false, url: 'https://hooks.slack.com/services/new' }
			}
		});
	});

	it('starts the bot token empty', () => {
		const draft = actionDraft({
			__typename: 'MonitorTelegram',
			id: 'T1',
			enabled: true,
			includeResults: true,
			botToken: '…3xyz',
			chatId: '@alerts'
		});
		expect(draft.masked).toBe('…3xyz');
		expect(actionEditInput(draft)).toEqual({
			telegram: { id: 'T1', update: { enabled: true, includeResults: true, botToken: '', chatId: '@alerts' } }
		});
	});

	it('starts the email header empty and keeps or removes the stored one', () => {
		const draft = actionDraft({
			__typename: 'MonitorEmail',
			id: 'E1',
			enabled: true,
			includeResults: false,
			priority: 'NORMAL',
			header: 'Approved: …word',
			recipients: { nodes: [{ id: 'U1' }] }
		} as Parameters<typeof actionDraft>[0]);
		expect(draft.header).toBe('');
		expect(draft.masked).toBe('Approved: …word');
		const update = { enabled: true, includeResults: false, priority: 'NORMAL', recipients: ['U1'] };
		expect(actionEditInput(draft)).toEqual({
			email: { id: 'E1', update: { ...update, header: '', removeHeader: false } }
		});
		draft.header = ' Approved: new-password ';
		expect(actionEditInput(draft)).toEqual({
			email: { id: 'E1', update: { ...update, header: 'Approved: new-password', removeHeader: false } }
		});
		draft.header = '';
		draft.removeHeader = true;
		expect(actionEditInput(draft)).toEqual({
			email: { id: 'E1', update: { ...update, header: '', removeHeader: true } }
		});
	});
});
