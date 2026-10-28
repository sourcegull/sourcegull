import { readdirSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { SITE_ADMIN_NAV } from '#lib/admin/nav.ts';
import { currentHref } from './side-nav.ts';

const items = SITE_ADMIN_NAV.flatMap((s) => s.items);

describe('currentHref', () => {
	it('picks the longest matching item', () => {
		expect(currentHref(items, '/site-admin/analytics/users')).toBe('/site-admin/analytics/users');
		expect(currentHref(items, '/site-admin/analytics')).toBe('/site-admin/analytics');
		expect(currentHref(items, '/site-admin/repositories/gitlab.com/a/b')).toBe(
			'/site-admin/repositories'
		);
	});
	it('matches exact items only on their own path', () => {
		expect(currentHref(items, '/site-admin')).toBe('/site-admin');
		expect(currentHref(items, '/site-admin/unknown')).toBeUndefined();
	});
	it('does not match a prefix that is not a path segment', () => {
		expect(currentHref([{ href: '/a/own', title: 'x' }], '/a/owner')).toBeUndefined();
	});
});

describe('SITE_ADMIN_NAV', () => {
	it('lists every page once', () => {
		const hrefs = items.map((i) => i.href);
		expect(new Set(hrefs).size).toBe(hrefs.length);
	});
	it('links only to existing site admin routes', () => {
		const dir = new URL('../../routes/(app)/site-admin/', import.meta.url);
		const routes = (readdirSync(dir, { recursive: true }) as string[])
			.filter((f) => f === '+page.svelte' || f.endsWith('/+page.svelte'))
			.map((f) => `/site-admin/${f}`.replace(/\/?\+page\.svelte$/, ''));
		for (const href of items.map((i) => i.href)) expect(routes).toContain(href);
	});
});
