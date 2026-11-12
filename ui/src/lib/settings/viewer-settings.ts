/** The viewer's merged settings, shared by the app layout and the pages that act on them. */
import { writable } from 'svelte/store';
import { applyDisplaySettings } from '#lib/format.svelte.ts';
import { graphql } from '#lib/graphql.ts';
import { VIEWER_SEARCH_SETTINGS, type ViewerSearchSettingsResult } from '#lib/search/queries.ts';

/** `viewerSettings.final`, or `{}` until it has loaded. */
export const viewerSettings = writable('{}');

/** Loads the settings; after a failure the last ones stay, since they only tune the UI. */
export async function refreshViewerSettings(): Promise<void> {
	try {
		const final = (await graphql<ViewerSearchSettingsResult>(VIEWER_SEARCH_SETTINGS)).viewerSettings.final;
		viewerSettings.set(final);
		applyDisplaySettings(final);
	} catch {
		// Keep what is there.
	}
}
