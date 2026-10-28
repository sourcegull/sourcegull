<!-- The badge of an object the resources file owns, in lists. -->
<script lang="ts">
	import { managedTitle } from '#lib/declared-config.ts';

	interface Props {
		/** The object's `managedBy`; nothing is shown when it is null. */
		managedBy: string | null | undefined;
	}

	let { managedBy }: Props = $props();
</script>

{#if managedBy}<span class="badge" title={managedTitle(managedBy)} data-managed-by={managedBy}>managed</span>{/if}
