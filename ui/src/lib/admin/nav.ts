/**
 * The site admin area's navigation: sections in the order an admin works
 * through them, most-used first. The sidebar (`SidebarLayout`) and the overview page's quick
 * links both read it.
 */
import type { NavSection } from '#lib/components/side-nav.ts';

export interface AdminNavSection extends NavSection {
	/** One line for the overview page's section card. */
	description: string;
}

export const SITE_ADMIN_NAV: AdminNavSection[] = [
	{
		title: 'Overview',
		description: 'Instance health and usage.',
		items: [
			{ href: '/site-admin', title: 'Overview', exact: true },
			{ href: '/site-admin/analytics', title: 'Analytics' },
			{ href: '/site-admin/analytics/users', title: 'User activity' }
		]
	},
	{
		title: 'Repositories',
		description: 'Connect GitLab, watch cloning and indexing, and manage repo nodes.',
		items: [
			{ href: '/site-admin/external-services', title: 'Code host connections' },
			{ href: '/site-admin/repositories', title: 'Repositories' },
			{ href: '/site-admin/repo-nodes', title: 'Repo nodes' },
			{ href: '/site-admin/permissions', title: 'Repository permissions' }
		]
	},
	{
		title: 'Users & access',
		description: 'Accounts, organizations, teams, roles and sign-up requests.',
		items: [
			{ href: '/site-admin/users', title: 'Users' },
			{ href: '/site-admin/organizations', title: 'Organizations' },
			{ href: '/site-admin/teams', title: 'Teams' },
			{ href: '/site-admin/roles', title: 'Roles' },
			{ href: '/site-admin/access-requests', title: 'Access requests' },
			{ href: '/site-admin/connected-apps', title: 'Connected apps' }
		]
	},
	{
		title: 'Configuration',
		description: 'Site configuration, settings for everyone, feature flags and webhooks.',
		items: [
			{ href: '/site-admin/configuration', title: 'Site configuration' },
			{ href: '/site-admin/global-settings', title: 'Global settings' },
			{ href: '/site-admin/feature-flags', title: 'Feature flags' },
			{ href: '/site-admin/outbound-webhooks', title: 'Outbound webhooks' }
		]
	},
	{
		title: 'Code intelligence',
		description: 'Precise code navigation: indexing policies, auto-indexing and executors.',
		items: [
			{ href: '/site-admin/code-graph', title: 'Code graph policies' },
			{ href: '/site-admin/code-graph/coverage', title: 'Index coverage' },
			{ href: '/site-admin/inference-script', title: 'Inference script' },
			{ href: '/site-admin/executors', title: 'Executors' },
			{ href: '/site-admin/executor-secrets', title: 'Executor secrets' }
		]
	},
	{
		title: 'Bulk changes',
		description: 'Who may run bulk changes, and the credentials they use.',
		items: [{ href: '/site-admin/bulk-changes', title: 'Bulk changes' }]
	},
	{
		title: 'Code ownership',
		description: 'CODEOWNERS files, ownership signals and coverage.',
		items: [
			{ href: '/site-admin/own/codeowners', title: 'CODEOWNERS files' },
			{ href: '/site-admin/own/signals', title: 'Ownership signals' },
			{ href: '/site-admin/own/analytics', title: 'Ownership analytics' }
		]
	},
	{
		title: 'Monitoring',
		description: 'Background jobs, slow and outbound requests, and incoming webhooks.',
		items: [
			{ href: '/site-admin/background-jobs', title: 'Background jobs' },
			{ href: '/site-admin/slow-requests', title: 'Slow requests' },
			{ href: '/site-admin/outbound-requests', title: 'Outbound requests' },
			{ href: '/site-admin/webhook-logs', title: 'Incoming webhook logs' }
		]
	},
	{
		title: 'Maintenance',
		description: 'Product version, upgrade readiness and data migrations.',
		items: [
			{ href: '/site-admin/updates', title: 'Updates' },
			{ href: '/site-admin/migrations', title: 'Migrations' }
		]
	}
];
