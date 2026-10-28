import type { IconName } from './Icon.svelte';

/** One link of the app sidebar. */
export interface NavItem {
	href: string;
	label: string;
	icon: IconName;
	/** Whether the item is the current section; defaults to a prefix match on `href`. */
	match?: (path: string) => boolean;
}

export interface NavGroup {
	/** Group heading; hidden in the collapsed rail. */
	label?: string;
	items: NavItem[];
}

export interface NavContext {
	signedIn: boolean;
	siteAdmin: boolean;
	batchChangesEnabled: boolean;
}

/** `/bulk-changes…` and a namespace's `/users/<name>/bulk-changes…`. */
export const isBatchChangesPath = (path: string) =>
	/^\/(?:(?:users|organizations)\/[^/]+\/)?bulk-changes(?:\/|$)/.test(path);

export function prefixMatch(href: string, path: string): boolean {
	return path === href || path.startsWith(`${href}/`);
}

export function isItemActive(item: NavItem, path: string): boolean {
	return item.match ? item.match(path) : prefixMatch(item.href, path);
}

/** The main navigation, by what the viewer may use. The site admin group goes last. */
export function mainNav(ctx: NavContext): NavGroup[] {
	const code: NavItem[] = [
		{ href: '/search', label: 'Search', icon: 'search' },
		{ href: '/repositories', label: 'Repositories', icon: 'repo' },
		{ href: '/contexts', label: 'Contexts', icon: 'context' },
		{ href: '/saved-searches', label: 'Saved searches', icon: 'bookmark' },
		{ href: '/notebooks', label: 'Notebooks', icon: 'notebook' },
		{ href: '/compute', label: 'Compute', icon: 'compute' }
	];
	if (ctx.signedIn) code.push({ href: '/search-jobs', label: 'Search jobs', icon: 'jobs' });
	code.push({ href: '/connect-agent', label: 'Connect an agent', icon: 'agent' });

	const automate: NavItem[] = [];
	if (ctx.signedIn) {
		automate.push({ href: '/watches', label: 'Watches', icon: 'monitor' });
		automate.push({ href: '/trends', label: 'Trends', icon: 'insights' });
	}
	if (ctx.batchChangesEnabled) {
		automate.push({ href: '/bulk-changes', label: 'Bulk changes', icon: 'batch', match: isBatchChangesPath });
	}

	const groups: NavGroup[] = [{ label: 'Code', items: code }];
	if (automate.length > 0) groups.push({ label: 'Automate', items: automate });
	if (ctx.siteAdmin) {
		groups.push({ label: 'Admin', items: [{ href: '/site-admin', label: 'Site admin', icon: 'admin' }] });
	}
	return groups;
}
