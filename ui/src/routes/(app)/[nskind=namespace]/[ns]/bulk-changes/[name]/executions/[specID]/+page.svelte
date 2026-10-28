<script lang="ts">
	import { page } from '$app/state';
	import ExecutionPage from '#lib/batches/ExecutionPage.svelte';
	import type { NamespaceKind } from '#lib/batches/spec.ts';

	const kind = $derived((page.params.nskind ?? 'users') as NamespaceKind);
</script>

{#key page.params.specID}
	<ExecutionPage {kind} namespace={page.params.ns ?? ''} name={page.params.name ?? ''} specID={page.params.specID ?? ''} />
{/key}
