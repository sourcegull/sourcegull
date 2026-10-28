<script lang="ts">
	import SidebarLayout from '#lib/components/SidebarLayout.svelte';
	import type { LayoutProps } from './$types';

	let { data, children }: LayoutProps = $props();

	const items = $derived([
		{ href: '/user/settings/profile', title: 'Profile' },
		...(data.auth.builtin ? [{ href: '/user/settings/password', title: 'Password' }] : []),
		{ href: '/user/settings/emails', title: 'Emails' },
		{ href: '/user/settings/tokens', title: 'Access tokens' },
		{ href: '/user/settings/connected-apps', title: 'Connected apps' },
		{ href: '/user/settings/executor-secrets', title: 'Executor secrets' },
		{ href: '/user/settings/organizations', title: 'Organizations' },
		...(data.auth.batchChangesEnabled ? [{ href: '/user/settings/bulk-changes', title: 'Bulk changes' }] : [])
	]);
</script>

<SidebarLayout title="Settings" {items}>
	{@render children()}
</SidebarLayout>
