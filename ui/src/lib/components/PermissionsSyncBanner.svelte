<!--
	Tells the signed-in user that their code host permissions are still syncing, so some of their
	repositories are not shown yet, and offers a reload once the sync finishes. Polls while the
	sync runs; failures are silent (the banner is a hint).
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { graphql } from '#lib/graphql.ts';
	import type { PermissionsSyncInProgressQuery } from '#lib/graphql-types.ts';
	import { PERMISSIONS_SYNC_IN_PROGRESS, PERMISSIONS_SYNC_POLL_INTERVAL } from '#lib/account/permissions-sync.ts';

	let phase = $state<'idle' | 'syncing' | 'done'>('idle');

	onMount(() => {
		let timer: ReturnType<typeof setTimeout> | undefined;
		let stopped = false;
		const check = () =>
			graphql<PermissionsSyncInProgressQuery>(PERMISSIONS_SYNC_IN_PROGRESS)
				.then((r) => {
					if (stopped) return;
					if (r.currentUser?.permissionsSyncInProgress) {
						phase = 'syncing';
						timer = setTimeout(check, PERMISSIONS_SYNC_POLL_INTERVAL);
					} else if (phase === 'syncing') {
						phase = 'done';
						void invalidateAll();
					}
				})
				.catch(() => {});
		void check();
		return () => {
			stopped = true;
			clearTimeout(timer);
		};
	});
</script>

{#if phase === 'syncing'}
	<div class="alert alert-info banner" role="status">
		Syncing your repository permissions from your code host. Repositories you have access to will appear
		when it finishes, usually within a minute.
	</div>
{:else if phase === 'done'}
	<div class="alert alert-success banner" role="status">
		Your repository permissions are synced.
		<button class="btn btn-sm" type="button" onclick={() => location.reload()}>Reload page</button>
		<button class="btn btn-ghost btn-sm" type="button" onclick={() => (phase = 'idle')}>Dismiss</button>
	</div>
{/if}

<style>
	.banner {
		margin: var(--space-3) var(--space-4) 0;
	}
	.banner button {
		margin-left: var(--space-2);
	}
</style>
