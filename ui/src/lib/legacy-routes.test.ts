import { describe, expect, it } from 'vitest';
import { legacyRedirect } from './legacy-routes.ts';

const go = (href: string) => legacyRedirect(new URL(href, 'https://sourcegull.example.com'));

describe('legacyRedirect', () => {
	it('maps renamed sections, keeping the rest of the path and the query', () => {
		expect(go('/insights')).toBe('/trends');
		expect(go('/insights/dashboards/RDE%3D')).toBe('/trends/dashboards/RDE%3D');
		expect(go('/insights/create/search?query=foo')).toBe('/trends/create/search?query=foo');
		expect(go('/code-monitoring')).toBe('/watches');
		expect(go('/code-monitoring/Q29kZU1vbml0b3I6MQ==/logs')).toBe('/watches/Q29kZU1vbml0b3I6MQ==/logs');
		expect(go('/batch-changes/create')).toBe('/bulk-changes/create');
		expect(go('/site-admin/batch-changes')).toBe('/site-admin/bulk-changes');
		expect(go('/user/settings/batch-changes')).toBe('/user/settings/bulk-changes');
	});

	it('maps namespace bulk change pages', () => {
		expect(go('/users/alice/batch-changes')).toBe('/users/alice/bulk-changes');
		expect(go('/users/alice/batch-changes/fix/edit')).toBe('/users/alice/bulk-changes/fix/edit');
		expect(go('/organizations/acme/batch-changes/apply/QmF0Y2hTcGVj')).toBe('/organizations/acme/bulk-changes/apply/QmF0Y2hTcGVj');
	});

	it('leaves other paths alone', () => {
		expect(go('/trends')).toBeNull();
		expect(go('/insightsfoo')).toBeNull();
		expect(go('/users/alice/settings')).toBeNull();
		expect(go('/gitlab.example.com/acme/repo/-/blob/insights/a.ts')).toBeNull();
		expect(go('/organizations/acme/batch-changes-x')).toBeNull();
	});
});
