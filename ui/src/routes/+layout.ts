import { redirect } from '@sveltejs/kit';
import { authRedirect, loadAuthState } from '#lib/auth.ts';
import { legacyRedirect } from '#lib/legacy-routes.ts';
import type { LayoutLoad } from './$types';

// Client-rendered SPA: the binary serves the `index.html` fallback for every UI route.
export const ssr = false;
export const prerender = false;
export const trailingSlash = 'never';

export const load: LayoutLoad = async ({ fetch, url, depends }) => {
	const moved = legacyRedirect(url);
	if (moved !== null) redirect(308, moved);
	depends('sourcegull:auth');
	const auth = await loadAuthState(fetch);
	const target = authRedirect(auth, url);
	if (target !== null && target !== url.pathname + url.search) redirect(307, target);
	return { auth };
};
