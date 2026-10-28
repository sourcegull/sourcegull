import { describe, expect, it } from 'vitest';
import { isBatchChangesPath, isItemActive, mainNav } from './nav.ts';

const labels = (ctx: Parameters<typeof mainNav>[0]) => mainNav(ctx).map((g) => [g.label, g.items.map((i) => i.label)]);

describe('mainNav', () => {
	it('shows only public sections when signed out', () => {
		expect(labels({ signedIn: false, siteAdmin: false, batchChangesEnabled: false })).toEqual([
			['Code', ['Search', 'Repositories', 'Contexts', 'Saved searches', 'Notebooks', 'Compute', 'Connect an agent']]
		]);
	});

	it('adds user sections, bulk changes and the admin group', () => {
		expect(labels({ signedIn: true, siteAdmin: true, batchChangesEnabled: true })).toEqual([
			[
				'Code',
				['Search', 'Repositories', 'Contexts', 'Saved searches', 'Notebooks', 'Compute', 'Search jobs', 'Connect an agent']
			],
			['Automate', ['Watches', 'Trends', 'Bulk changes']],
			['Admin', ['Site admin']]
		]);
	});
});

describe('active item', () => {
	const groups = mainNav({ signedIn: true, siteAdmin: true, batchChangesEnabled: true });
	const items = groups.flatMap((g) => g.items);
	const active = (path: string) => items.filter((i) => isItemActive(i, path)).map((i) => i.label);

	it('matches by path prefix on segment boundaries', () => {
		expect(active('/search')).toEqual(['Search']);
		expect(active('/search-jobs/12')).toEqual(['Search jobs']);
		expect(active('/compute')).toEqual(['Compute']);
		expect(active('/site-admin/repositories')).toEqual(['Site admin']);
		expect(active('/gitlab.example.com/acme/repo')).toEqual([]);
	});

	it('treats namespace bulk changes as bulk changes', () => {
		expect(isBatchChangesPath('/users/alice/bulk-changes/foo')).toBe(true);
		expect(isBatchChangesPath('/organizations/acme/bulk-changes')).toBe(true);
		expect(isBatchChangesPath('/users/alice/settings')).toBe(false);
		expect(active('/users/alice/bulk-changes/x')).toEqual(['Bulk changes']);
	});
});
