/**
 * GraphQL operations and helpers of the site admin Outbound webhooks pages: webhooks with their event types, HMAC secret and delivery logs,
 * and the delivery log of watch actions that post to a URL. Webhooks the resources file owns are read-only.
 */
import { DECLARED_KINDS_FIELDS, type DeclaredKindsFile } from '#lib/declared-config.ts';
import type {
	ChannelDeliveryLogsQuery,
	DeliveryChannel,
	OutboundWebhookEventTypesQuery,
	OutboundWebhookQuery,
	OutboundWebhooksQuery
} from '#lib/graphql-types.ts';

export const OUTBOUND_WEBHOOKS = `query OutboundWebhooks($first: Int, $after: String) {
	outboundWebhooks(first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes {
			id
			url
			eventTypes { eventType scope }
			stats { total errored }
			createdAt
			updatedAt
			managedBy
		}
	}
	${DECLARED_KINDS_FIELDS}
}`;

export const OUTBOUND_WEBHOOK_EVENT_TYPES = `query OutboundWebhookEventTypes {
	outboundWebhookEventTypes { key description }
}`;

export const OUTBOUND_WEBHOOK = `query OutboundWebhook($id: ID!, $first: Int, $after: String, $onlyErrors: Boolean) {
	node(id: $id) {
		__typename
		... on OutboundWebhook {
			id
			url
			eventTypes { eventType scope }
			stats { total errored }
			createdAt
			updatedAt
			managedBy
			createdBy { username }
			updatedBy { username }
			logs(first: $first, after: $after, onlyErrors: $onlyErrors) {
				totalCount
				pageInfo { hasNextPage endCursor }
				nodes {
					id
					sentAt
					statusCode
					error
					job { eventType scope payload }
					request { method url headers { name values } body }
					response { statusCode headers { name values } body }
				}
			}
		}
	}
}`;

export const CHANNEL_DELIVERY_LOGS = `query ChannelDeliveryLogs($first: Int, $after: String, $onlyErrors: Boolean) {
	channelDeliveryLogs(first: $first, after: $after, onlyErrors: $onlyErrors) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { id sentAt statusCode error channel host watchId watchDescription }
	}
}`;

export type ChannelDeliveryLogsResult = ChannelDeliveryLogsQuery;

export type ChannelDeliveryLog = ChannelDeliveryLogsQuery['channelDeliveryLogs']['nodes'][number];

const CHANNEL_NAMES: Record<DeliveryChannel, string> = {
	WEBHOOK: 'Webhook',
	SLACK_WEBHOOK: 'Slack',
	MATTERMOST_WEBHOOK: 'Mattermost',
	TELEGRAM: 'Telegram'
};

/** The channel in words; `Unknown` for one the API did not name. */
export function channelName(channel: DeliveryChannel | null): string {
	return channel ? CHANNEL_NAMES[channel] : 'Unknown';
}

export const CREATE_OUTBOUND_WEBHOOK = `mutation CreateOutboundWebhook($input: OutboundWebhookCreateInput!) {
	createOutboundWebhook(input: $input) { id }
}`;

export const UPDATE_OUTBOUND_WEBHOOK = `mutation UpdateOutboundWebhook($id: ID!, $input: OutboundWebhookUpdateInput!) {
	updateOutboundWebhook(id: $id, input: $input) { id }
}`;

export const DELETE_OUTBOUND_WEBHOOK = `mutation DeleteOutboundWebhook($id: ID!) {
	deleteOutboundWebhook(id: $id) { alwaysNil }
}`;

export interface ScopedEventType {
	eventType: string;
	scope: string | null;
}

export interface EventTypeInfo {
	key: string;
	description: string;
}

export interface OutboundWebhookSummary {
	id: string;
	url: string;
	eventTypes: ScopedEventType[];
	stats: { total: number; errored: number };
	createdAt: string;
	updatedAt: string;
	/** The resources file entry that owns the webhook. */
	managedBy: string | null;
}

export type OutboundWebhooksResult = OutboundWebhooksQuery;

export type EventTypesResult = OutboundWebhookEventTypesQuery;

export interface Header {
	name: string;
	values: string[];
}

export interface DeliveryLog {
	id: string;
	sentAt: string;
	statusCode: number;
	error: string | null;
	job: { eventType: string; scope: string | null; payload: string };
	request: { method: string; url: string; headers: Header[]; body: string };
	response: { statusCode: number; headers: Header[]; body: string } | null;
}

export interface OutboundWebhookDetail extends OutboundWebhookSummary {
	createdBy: { username: string } | null;
	updatedBy: { username: string } | null;
	logs: {
		totalCount: number;
		pageInfo: { hasNextPage: boolean; endCursor: string | null };
		nodes: DeliveryLog[];
	};
}

export type OutboundWebhookResult = OutboundWebhookQuery;

/** The form's event type selection as API input: one entry per checked type, keeping scopes. */
export function eventTypesInput(
	checked: Record<string, boolean>,
	scopes: Record<string, string>
): ScopedEventType[] {
	return Object.keys(checked)
		.filter((k) => checked[k])
		.sort()
		.map((eventType) => {
			const scope = (scopes[eventType] ?? '').trim();
			return { eventType, scope: scope === '' ? null : scope };
		});
}

/** Validates the form; returns the problems. Editing, an empty URL keeps the stored one. */
export function validateWebhookForm(
	url: string,
	secret: string,
	isNew: boolean,
	types: ScopedEventType[]
): string[] {
	const problems: string[] = [];
	if (isNew || url.trim() !== '') {
		try {
			const u = new URL(url.trim());
			if (u.protocol !== 'http:' && u.protocol !== 'https:') problems.push('The URL must be http or https.');
		} catch {
			problems.push('Enter a valid URL.');
		}
	}
	if (isNew && secret.trim() === '') problems.push('Enter a secret.');
	if (types.length === 0) problems.push('Choose at least one event type.');
	return problems;
}

/** Event types in words, e.g. `changeset:publish, changeset:close (scope x)`. */
export function describeEventTypes(types: ScopedEventType[]): string {
	return types.map((t) => (t.scope ? `${t.eventType} (${t.scope})` : t.eventType)).join(', ');
}
