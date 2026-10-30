/**
 * Links between site admin pages and the site configuration sections that control them, so an
 * admin can move between a page and its settings without searching the sidebar.
 * `RelatedLinks` shows them under a page's header; the configuration page shows the pages of
 * the selected section.
 */

export interface RelatedLink {
	href: string;
	title: string;
}

/** Section ids of the site configuration (`crates/config/src/sections.rs`). */
export type ConfigSectionId =
	| 'general'
	| 'auth'
	| 'permissions'
	| 'repositories'
	| 'search'
	| 'codeIntel'
	| 'batchChanges'
	| 'insights'
	| 'notifications'
	| 'observability';

/** The configuration page opened at one section. */
export function configSectionURL(id: ConfigSectionId): string {
	return `/site-admin/configuration?section=${id}`;
}

const cfg = (id: ConfigSectionId, title: string): RelatedLink => ({ href: configSectionURL(id), title });
const page = (href: string, title: string): RelatedLink => ({ href, title });

const AUTH = cfg('auth', 'Authentication & sign-in settings');
const PERMISSIONS = cfg('permissions', 'Users & permissions settings');
const REPOSITORIES = cfg('repositories', 'Repositories & git settings');
const SEARCH = cfg('search', 'Search & indexing settings');
const CODE_INTEL = cfg('codeIntel', 'Code intelligence settings');
const OBSERVABILITY = cfg('observability', 'Observability & logs settings');

const USERS = page('/site-admin/users', 'Users');
const ACCESS_REQUESTS = page('/site-admin/access-requests', 'Access requests');
const ROLES = page('/site-admin/roles', 'Roles');
const ORGS = page('/site-admin/organizations', 'Organizations');
const TEAMS = page('/site-admin/teams', 'Teams');
const CONNECTED_APPS = page('/site-admin/connected-apps', 'Connected apps');
const CONNECTIONS = page('/site-admin/external-services', 'Code host connections');
const REPOS = page('/site-admin/repositories', 'Repositories');
const REPO_NODES = page('/site-admin/repo-nodes', 'Repo nodes');
const REPO_PERMISSIONS = page('/site-admin/permissions', 'Repository permissions');
const WEBHOOK_LOGS = page('/site-admin/webhook-logs', 'Incoming webhook logs');
const CODE_GRAPH = page('/site-admin/code-graph', 'Code graph policies');
const COVERAGE = page('/site-admin/code-graph/coverage', 'Index coverage');
const INFERENCE = page('/site-admin/inference-script', 'Inference script');
const EXECUTORS = page('/site-admin/executors', 'Executors');
const EXECUTOR_SECRETS = page('/site-admin/executor-secrets', 'Executor secrets');
const OUTBOUND_WEBHOOKS = page('/site-admin/outbound-webhooks', 'Outbound webhooks');

const CONNECTION_LINKS = [AUTH, REPO_PERMISSIONS, REPOS, REPOSITORIES, WEBHOOK_LOGS];
const CODE_INTEL_LINKS = [CODE_GRAPH, COVERAGE, INFERENCE, EXECUTORS, EXECUTOR_SECRETS, CODE_INTEL];

/** Related links per site admin route (SvelteKit route id without the `(app)` group). */
export const BY_ROUTE: Record<string, RelatedLink[]> = {
	'/site-admin/users': [AUTH, ACCESS_REQUESTS, ROLES, ORGS, TEAMS],
	'/site-admin/access-requests': [AUTH, USERS],
	'/site-admin/roles': [USERS, PERMISSIONS],
	'/site-admin/organizations': [USERS, TEAMS],
	'/site-admin/teams': [USERS, ORGS, PERMISSIONS],
	'/site-admin/connected-apps': [AUTH, page('/connect-agent', 'Connect an agent')],
	'/site-admin/external-services': CONNECTION_LINKS,
	'/site-admin/external-services/new': CONNECTION_LINKS,
	'/site-admin/external-services/[id]': CONNECTION_LINKS,
	'/site-admin/external-services/[id]/edit': CONNECTION_LINKS,
	'/site-admin/repositories': [CONNECTIONS, REPO_NODES, REPOSITORIES, SEARCH],
	'/site-admin/repositories/[...name]': [REPOS, REPO_PERMISSIONS, REPOSITORIES, SEARCH],
	'/site-admin/repo-nodes': [REPOS, REPOSITORIES],
	'/site-admin/permissions': [CONNECTIONS, USERS, PERMISSIONS],
	'/site-admin/webhook-logs': [CONNECTIONS],
	'/site-admin/code-graph': CODE_INTEL_LINKS,
	'/site-admin/code-graph/coverage': CODE_INTEL_LINKS,
	'/site-admin/inference-script': CODE_INTEL_LINKS,
	'/site-admin/executors': CODE_INTEL_LINKS,
	'/site-admin/executor-secrets': CODE_INTEL_LINKS,
	'/site-admin/bulk-changes': [cfg('batchChanges', 'Bulk changes settings'), CONNECTIONS],
	'/site-admin/outbound-webhooks': [cfg('notifications', 'Email & webhooks settings')],
	'/site-admin/outbound-webhooks/[id]': [OUTBOUND_WEBHOOKS, cfg('notifications', 'Email & webhooks settings')],
	'/site-admin/outbound-webhooks/new': [OUTBOUND_WEBHOOKS, cfg('notifications', 'Email & webhooks settings')],
	'/site-admin/analytics': [OBSERVABILITY],
	'/site-admin/analytics/users': [OBSERVABILITY],
	'/site-admin/slow-requests': [OBSERVABILITY],
	'/site-admin/outbound-requests': [OBSERVABILITY]
};

/** Pages controlled by each configuration section, shown with the section. */
const BY_SECTION: Partial<Record<ConfigSectionId, RelatedLink[]>> = {
	auth: [USERS, ACCESS_REQUESTS, CONNECTED_APPS, CONNECTIONS],
	permissions: [REPO_PERMISSIONS, USERS, ROLES, TEAMS],
	repositories: [CONNECTIONS, REPOS, REPO_NODES],
	search: [REPOS],
	codeIntel: [CODE_GRAPH, COVERAGE, EXECUTORS, EXECUTOR_SECRETS],
	batchChanges: [page('/site-admin/bulk-changes', 'Bulk changes')],
	notifications: [OUTBOUND_WEBHOOKS],
	observability: [page('/site-admin/slow-requests', 'Slow requests'), page('/site-admin/outbound-requests', 'Outbound requests')]
};

/** The related links of the page at SvelteKit route `routeId`; empty for other pages. */
export function relatedForRoute(routeId: string | null): RelatedLink[] {
	if (!routeId) return [];
	return BY_ROUTE[routeId.replace(/^\/\([^)]+\)/, '')] ?? [];
}

/** The pages related to configuration section `id`. */
export function relatedForSection(id: string): RelatedLink[] {
	return BY_SECTION[id as ConfigSectionId] ?? [];
}
