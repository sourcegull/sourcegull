<script lang="ts">
	import { page } from '$app/state';
	import EditSpecPage from '#lib/batches/EditSpecPage.svelte';
	import type { NamespaceKind } from '#lib/batches/spec.ts';

	const kind = $derived((page.params.nskind ?? 'users') as NamespaceKind);
</script>

{#key `${kind}/${page.params.ns}/${page.params.name}`}
	<EditSpecPage {kind} namespace={page.params.ns ?? ''} name={page.params.name ?? ''} />
{/key}
