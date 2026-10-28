<script lang="ts">
	import SidebarLayout from '#lib/components/SidebarLayout.svelte';
	import { SITE_ADMIN_NAV } from '#lib/admin/nav.ts';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<SidebarLayout title="Site admin" sections={SITE_ADMIN_NAV}>
	{@render children()}
</SidebarLayout>
