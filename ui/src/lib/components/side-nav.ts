/** Items and sections of an in-page side navigation (`SideNav`, `SidebarLayout`). */

export interface NavItem {
	href: string;
	title: string;
	/** Current only on exactly this path, not on the paths below it (e.g. a section's index). */
	exact?: boolean;
}

export interface NavSection {
	/** Group label; a section without one is shown without a heading. */
	title?: string;
	items: NavItem[];
}

function matches(item: NavItem, pathname: string): boolean {
	if (pathname === item.href) return true;
	return !item.exact && pathname.startsWith(`${item.href}/`);
}

/**
 * The href of the current item: the longest one matching the path, so that
 * `/site-admin/analytics/users` marks "User activity", not also "Analytics".
 */
export function currentHref(items: NavItem[], pathname: string): string | undefined {
	let best: NavItem | undefined;
	for (const item of items) {
		if (matches(item, pathname) && (!best || item.href.length > best.href.length)) best = item;
	}
	return best?.href;
}
