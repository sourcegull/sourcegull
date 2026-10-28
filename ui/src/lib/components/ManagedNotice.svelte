<!--
	The notice on pages of objects the resources file (SOURCEGULL_RESOURCES_FILE) owns: one object
	(`managedBy`), or every object of a kind (`kind`, when the file is exclusive for it).
-->
<script lang="ts">
	import { kindLabel, managesWholeKind, type DeclaredKind } from '#lib/declared-config.ts';

	interface Props {
		/** The object's `managedBy`: `<kind>[<key>]`, or the bare kind. */
		managedBy?: string | null;
		/** A kind the file owns exclusively, for pages that would create one. */
		kind?: DeclaredKind;
	}

	let { managedBy = null, kind }: Props = $props();

	const label = $derived(kind ? kindLabel(kind) : '');
</script>

{#if kind}
	<div class="alert alert-info" role="status" data-testid="managed-notice">
		{label.charAt(0).toUpperCase()}{label.slice(1)} are managed by SOURCEGULL_RESOURCES_FILE
		(<code class="code-inline">{kind}</code>), so they can't be added or changed here. Add them to the file.
	</div>
{:else if managedBy}
	<div class="alert alert-info" role="status" data-testid="managed-notice">
		Managed by SOURCEGULL_RESOURCES_FILE (<code class="code-inline">{managedBy}</code>{managesWholeKind(managedBy)
			? ', which owns all of them'
			: ''}). Change it in the file.
	</div>
{/if}
