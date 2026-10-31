/**
 * Whether the signed-in user's permissions sync is still running (one starts at every sign-in),
 * so the app can say that more repositories are about to show up.
 */

export const PERMISSIONS_SYNC_IN_PROGRESS = `query PermissionsSyncInProgress {
	currentUser {
		permissionsSyncInProgress
	}
}`;

/** How often the banner asks again while the sync runs (ms). */
export const PERMISSIONS_SYNC_POLL_INTERVAL = 3_000;
