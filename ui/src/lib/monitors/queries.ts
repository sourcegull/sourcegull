/**
 * GraphQL operations and types of the watches pages.
 */
import { graphql } from '#lib/graphql.ts';
import { VIEWER_NAMESPACES, type NamespaceRef, type ViewerNamespacesResult } from '#lib/search/queries.ts';
import type {
	CanShareUnfilteredWatchResultsQuery,
	CodeMonitorEventsQuery,
	CodeMonitorQuery,
	UserMonitorsQuery
} from '#lib/graphql-types.ts';

export type EventStatus = 'PENDING' | 'SUCCESS' | 'ERROR';
export type EmailPriority = 'NORMAL' | 'CRITICAL';

export interface MonitorEmailAction {
	__typename: 'MonitorEmail';
	id: string;
	enabled: boolean;
	includeResults: boolean;
	priority: EmailPriority;
	header: string;
	recipients: { nodes: NamespaceRef[] };
}

export interface MonitorWebhookAction {
	__typename: 'MonitorWebhook' | 'MonitorSlackWebhook' | 'MonitorMattermostWebhook';
	id: string;
	enabled: boolean;
	includeResults: boolean;
	url: string;
}

export interface MonitorTelegramAction {
	__typename: 'MonitorTelegram';
	id: string;
	enabled: boolean;
	includeResults: boolean;
	botToken: string;
	chatId: string;
}

export type MonitorActionNode = MonitorEmailAction | MonitorWebhookAction | MonitorTelegramAction;

export interface MonitorNode {
	id: string;
	description: string;
	enabled: boolean;
	createdAt: string;
	url: string;
	owner: NamespaceRef;
	/** Whose repository permissions the watch searches with. */
	runsAs: { username: string } | null;
	/** Every email recipient gets all results, found with the access of `runsAs`. */
	unfilteredResults: boolean;
	unfilteredBy: { username: string } | null;
	unfilteredAt: string | null;
	trigger: { id: string; query: string };
	actions: { nodes: MonitorActionNode[] };
}

const ACTION_FIELDS = `
				__typename
				... on MonitorEmail {
					id enabled includeResults priority header
					recipients { nodes { __typename id ... on User { username } ... on Org { name } } }
				}
				... on MonitorWebhook { id enabled includeResults url }
				... on MonitorSlackWebhook { id enabled includeResults url }
				... on MonitorMattermostWebhook { id enabled includeResults url }
				... on MonitorTelegram { id enabled includeResults botToken chatId }`;

const MONITOR_FIELDS = `
		id
		description
		enabled
		createdAt
		url
		owner { __typename id ... on User { username } ... on Org { name } }
		runsAs { username }
		unfilteredResults
		unfilteredBy { username }
		unfilteredAt
		trigger { ... on MonitorQuery { id query } }
		actions(first: 50) { nodes {${ACTION_FIELDS}
		} }`;

export const USER_MONITORS = `query UserMonitors($user: ID!, $first: Int!, $after: String) {
	node(id: $user) {
		... on User {
			monitors(first: $first, after: $after) {
				totalCount
				pageInfo { hasNextPage endCursor }
				nodes {${MONITOR_FIELDS}
				}
			}
		}
	}
}`;

export type UserMonitorsResult = UserMonitorsQuery;

export const MONITOR = `query CodeMonitor($id: ID!) {
	node(id: $id) {
		__typename
		... on Monitor {${MONITOR_FIELDS}
		}
	}
}`;

export type MonitorResult = CodeMonitorQuery;

export const MONITOR_EVENTS = `query CodeMonitorEvents($id: ID!, $first: Int!, $after: String) {
	node(id: $id) {
		... on Monitor {
			description
			trigger {
				... on MonitorQuery {
					query
					events(first: $first, after: $after) {
						totalCount
						pageInfo { hasNextPage endCursor }
						nodes {
							id status message timestamp resultCount query
							ranges { repository revision from to }
							actions {
								nodes {
									__typename
									... on MonitorEmail { id events { nodes { id status message timestamp } } }
									... on MonitorWebhook { id events { nodes { id status message timestamp } } }
									... on MonitorSlackWebhook { id events { nodes { id status message timestamp } } }
									... on MonitorMattermostWebhook { id events { nodes { id status message timestamp } } }
									... on MonitorTelegram { id events { nodes { id status message timestamp } } }
								}
							}
						}
					}
				}
			}
		}
	}
}`;

export interface ActionEventNode {
	id: string;
	status: EventStatus;
	message: string | null;
	timestamp: string;
}

export interface TriggerEventNode {
	id: string;
	status: EventStatus;
	message: string | null;
	timestamp: string;
	resultCount: number;
	query: string | null;
	ranges: { repository: string; revision: string; from: string | null; to: string }[];
	actions: { nodes: { __typename: MonitorActionNode['__typename']; id: string; events: { nodes: ActionEventNode[] } }[] };
}

export type MonitorEventsResult = CodeMonitorEventsQuery;

export const CREATE_MONITOR = `mutation CreateCodeMonitor($monitor: MonitorInput!, $trigger: MonitorTriggerInput!, $actions: [MonitorActionInput!]!) {
	createCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) { id url }
}`;

export const UPDATE_MONITOR = `mutation UpdateCodeMonitor($monitor: MonitorEditInput!, $trigger: MonitorEditTriggerInput!, $actions: [MonitorEditActionInput!]!) {
	updateCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) { id url }
}`;

export const CAN_SHARE_UNFILTERED = `query CanShareUnfilteredWatchResults {
	viewerCanShareUnfilteredWatchResults
}`;

/** Whether the viewer may make a watch send unfiltered results. */
export async function canShareUnfiltered(): Promise<boolean> {
	const r = await graphql<CanShareUnfilteredWatchResultsQuery>(CAN_SHARE_UNFILTERED);
	return r.viewerCanShareUnfilteredWatchResults;
}

/** Whether a namespace ID is an organization's. */
export function isOrgId(id: string): boolean {
	try {
		return atob(id.replace(/-/g, '+').replace(/_/g, '/')).startsWith('Org:');
	} catch {
		return false;
	}
}

export const TOGGLE_MONITOR = `mutation ToggleCodeMonitor($id: ID!, $enabled: Boolean!) {
	toggleCodeMonitor(id: $id, enabled: $enabled) { id enabled }
}`;

export const DELETE_MONITOR = `mutation DeleteCodeMonitor($id: ID!) {
	deleteCodeMonitor(id: $id) { alwaysNil }
}`;

export const RESET_TRIGGER = `mutation ResetCodeMonitorTrigger($id: ID!) {
	resetTriggerQueryTimestamps(id: $id) { alwaysNil }
}`;

export const TEST_EMAIL = `mutation TestCodeMonitorEmail($namespace: ID!, $description: String!, $email: MonitorEmailInput!, $action: ID) {
	triggerTestEmailAction(namespace: $namespace, description: $description, email: $email, action: $action) { alwaysNil }
}`;

export const TEST_WEBHOOK = `mutation TestCodeMonitorWebhook($namespace: ID!, $description: String!, $webhook: MonitorWebhookInput!, $action: ID) {
	triggerTestWebhookAction(namespace: $namespace, description: $description, webhook: $webhook, action: $action) { alwaysNil }
}`;

export const TEST_SLACK_WEBHOOK = `mutation TestCodeMonitorSlackWebhook($namespace: ID!, $description: String!, $slackWebhook: MonitorSlackWebhookInput!, $action: ID) {
	triggerTestSlackWebhookAction(namespace: $namespace, description: $description, slackWebhook: $slackWebhook, action: $action) { alwaysNil }
}`;

export const TEST_MATTERMOST_WEBHOOK = `mutation TestCodeMonitorMattermostWebhook($namespace: ID!, $description: String!, $mattermostWebhook: MonitorMattermostWebhookInput!, $action: ID) {
	triggerTestMattermostWebhookAction(namespace: $namespace, description: $description, mattermostWebhook: $mattermostWebhook, action: $action) { alwaysNil }
}`;

export const TEST_TELEGRAM = `mutation TestCodeMonitorTelegram($namespace: ID!, $description: String!, $telegram: MonitorTelegramInput!, $action: ID) {
	triggerTestTelegramAction(namespace: $namespace, description: $description, telegram: $telegram, action: $action) { alwaysNil }
}`;

/** An action being edited in the form. */
export interface ActionDraft {
	kind: 'email' | 'webhook' | 'slackWebhook' | 'mattermostWebhook' | 'telegram';
	/** Set for stored actions. */
	id: string | null;
	enabled: boolean;
	includeResults: boolean;
	url: string;
	priority: EmailPriority;
	/** Email: a new header; starts empty for stored actions (left empty, the stored one is kept). */
	header: string;
	/** Email: remove the stored header. */
	removeHeader: boolean;
	/** Recipient namespace IDs (email). */
	recipients: string[];
	/** Telegram. */
	botToken: string;
	chatId: string;
	/**
	 * Stored actions: the URL, bot token or email header as the server returns it, masked.
	 * `url`, `botToken` and `header` start empty; left empty, the stored secret is kept.
	 */
	masked: string;
}

/** The action kind of each GraphQL action type. */
const KIND_OF: Record<MonitorActionNode['__typename'], ActionDraft['kind']> = {
	MonitorEmail: 'email',
	MonitorWebhook: 'webhook',
	MonitorSlackWebhook: 'slackWebhook',
	MonitorMattermostWebhook: 'mattermostWebhook',
	MonitorTelegram: 'telegram'
};

export function actionDraft(a: MonitorActionNode): ActionDraft {
	if (a.__typename === 'MonitorEmail') {
		return {
			kind: 'email',
			id: a.id,
			enabled: a.enabled,
			includeResults: a.includeResults,
			url: '',
			priority: a.priority,
			header: '',
			removeHeader: false,
			recipients: a.recipients.nodes.map((r) => r.id),
			botToken: '',
			chatId: '',
			masked: a.header
		};
	}
	if (a.__typename === 'MonitorTelegram') {
		return {
			kind: 'telegram',
			id: a.id,
			enabled: a.enabled,
			includeResults: a.includeResults,
			url: '',
			priority: 'NORMAL',
			header: '',
			removeHeader: false,
			recipients: [],
			botToken: '',
			chatId: a.chatId,
			masked: a.botToken
		};
	}
	return {
		kind: KIND_OF[a.__typename],
		id: a.id,
		enabled: a.enabled,
		includeResults: a.includeResults,
		url: '',
		priority: 'NORMAL',
		header: '',
		removeHeader: false,
		recipients: [],
		botToken: '',
		chatId: '',
		masked: a.url
	};
}

/** The create input of a draft (`MonitorActionInput`). */
export function actionInput(a: ActionDraft): Record<string, unknown> {
	if (a.kind === 'email') {
		return {
			email: {
				enabled: a.enabled,
				includeResults: a.includeResults,
				priority: a.priority,
				recipients: a.recipients,
				header: a.header.trim(),
				removeHeader: a.removeHeader
			}
		};
	}
	if (a.kind === 'telegram') {
		return {
			telegram: {
				enabled: a.enabled,
				includeResults: a.includeResults,
				botToken: a.botToken.trim(),
				chatId: a.chatId.trim()
			}
		};
	}
	return { [a.kind]: { enabled: a.enabled, includeResults: a.includeResults, url: a.url.trim() } };
}

/** The update input of a draft (`MonitorEditActionInput`): no `id` creates the action. */
export function actionEditInput(a: ActionDraft): Record<string, unknown> {
	const kind = a.kind;
	return { [kind]: { id: a.id, update: actionInput(a)[kind] } };
}

/** The label of an action kind. */
export function actionLabel(kind: ActionDraft['kind'] | MonitorActionNode['__typename']): string {
	switch (kind in KIND_OF ? KIND_OF[kind as MonitorActionNode['__typename']] : kind) {
		case 'email':
			return 'Email';
		case 'webhook':
			return 'Webhook';
		case 'mattermostWebhook':
			return 'Mattermost webhook';
		case 'telegram':
			return 'Telegram';
		default:
			return 'Slack webhook';
	}
}

/** Whether a query is a commit or diff search, as watches require. */
export function isMonitorQuery(query: string): boolean {
	return /(^|[\s(])type:(commit|diff)\b/i.test(query);
}

/** The namespaces the viewer may act for: themself, then their organizations. */
export async function viewerNamespaces(): Promise<{ id: string; label: string }[]> {
	const r = await graphql<ViewerNamespacesResult>(VIEWER_NAMESPACES);
	const u = r.currentUser;
	if (!u) return [];
	return [
		{ id: u.id, label: `@${u.username}` },
		...u.organizations.nodes.map((o) => ({ id: o.id, label: `@${o.name}` }))
	];
}
