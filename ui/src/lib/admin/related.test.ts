import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { BY_ROUTE, configSectionURL, relatedForRoute, relatedForSection, type RelatedLink } from './related.ts';

const ROUTES = new URL('../../routes/(app)', import.meta.url);
const SECTION_IDS = ['general', 'auth', 'permissions', 'repositories', 'search', 'codeIntel', 'batchChanges', 'insights', 'notifications', 'observability'];

/** Whether `href` names a page of the app (or a configuration section the server defines). */
function resolves(link: RelatedLink): boolean {
	const url = new URL(link.href, 'http://x');
	if (url.pathname === '/site-admin/configuration') return SECTION_IDS.includes(url.searchParams.get('section') ?? '');
	return existsSync(new URL(`.${url.pathname}/+page.svelte`, `${ROUTES.href}/`));
}

describe('related links', () => {
	it('looks routes up without their group', () => {
		expect(relatedForRoute('/(app)/site-admin/users').map((l) => l.href)).toContain(configSectionURL('auth'));
		expect(relatedForRoute('/(app)/site-admin/external-services/new').map((l) => l.href)).toContain(
			configSectionURL('auth')
		);
		expect(relatedForRoute('/(app)/search')).toEqual([]);
		expect(relatedForRoute(null)).toEqual([]);
	});

	it('lists routes that exist', () => {
		const missing = Object.keys(BY_ROUTE).filter((r) => !existsSync(new URL(`.${r}/+page.svelte`, `${ROUTES.href}/`)));
		expect(missing).toEqual([]);
	});

	it('links only to pages that exist', () => {
		const links = [
			...Object.values(BY_ROUTE).flat(),
			...SECTION_IDS.flatMap((id) => relatedForSection(id))
		];
		expect(links.length).toBeGreaterThan(20);
		expect(links.filter((l) => !resolves(l))).toEqual([]);
	});

	it('uses the section ids of the server', () => {
		const rust = readFileSync(new URL('../../../../crates/config/src/sections.rs', import.meta.url), 'utf8');
		for (const id of SECTION_IDS) expect(rust).toContain(`id: "${id}"`);
	});
});
