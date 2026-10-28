import { error } from '@sveltejs/kit';
import { canAccessSiteAdmin } from '#lib/auth.ts';
import type { LayoutLoad } from './$types';

export const load: LayoutLoad = async ({ parent }) => {
	const { auth } = await parent();
	if (!canAccessSiteAdmin(auth)) error(403, 'Only site admins can view this page.');
};
