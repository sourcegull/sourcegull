/** Former UI path prefixes and the prefixes that replaced them. */
const RENAMED: [RegExp, string][] = [
	[/^\/insights(?=\/|$)/, '/trends'],
	[/^\/code-monitoring(?=\/|$)/, '/watches'],
	[/^\/batch-changes(?=\/|$)/, '/bulk-changes'],
	[/^\/site-admin\/batch-changes(?=\/|$)/, '/site-admin/bulk-changes'],
	[/^\/user\/settings\/batch-changes(?=\/|$)/, '/user/settings/bulk-changes'],
	[/^(\/(?:users|organizations)\/[^/]+)\/batch-changes(?=\/|$)/, '$1/bulk-changes']
];

/**
 * The current location of a page whose path was renamed (`/insights/…` is `/trends/…`), keeping
 * the rest of the path and the query; `null` for any other path.
 */
export function legacyRedirect(url: Pick<URL, 'pathname' | 'search'>): string | null {
	for (const [from, to] of RENAMED) {
		if (from.test(url.pathname)) return url.pathname.replace(from, to) + url.search;
	}
	return null;
}
