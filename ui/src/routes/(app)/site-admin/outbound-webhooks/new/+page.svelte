<!--
	Create an outbound webhook. When the resources file owns every webhook, the page says so
	instead of offering the form.
-->
<script lang="ts">
	import type { CreateOutboundWebhookMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import OutboundWebhookForm from '#lib/admin/OutboundWebhookForm.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { DECLARED_KINDS, isExclusive, type DeclaredKindsResult } from '#lib/declared-config.ts';
	import { graphql } from '#lib/graphql.ts';
	import { CREATE_OUTBOUND_WEBHOOK } from '#lib/admin/outbound-webhooks.ts';

	let exclusive = $state(false);

	onMount(async () => {
		try {
			const r = await graphql<DeclaredKindsResult>(DECLARED_KINDS);
			exclusive = isExclusive(r.site.declaredConfigFiles, 'outboundWebhooks');
		} catch {
			// The form stays; a save the server refuses shows its error.
		}
	});

	async function create(input: {
		url: string;
		secret: string | null;
		eventTypes: { eventType: string; scope: string | null }[];
	}): Promise<void> {
		const r = await graphql<CreateOutboundWebhookMutation, { input: unknown }>(
			CREATE_OUTBOUND_WEBHOOK,
			{ input: { url: input.url, secret: input.secret ?? '', eventTypes: input.eventTypes } }
		);
		await goto(`/site-admin/outbound-webhooks/${encodeURIComponent(r.createOutboundWebhook.id)}`);
	}
</script>

<svelte:head><title>New outbound webhook · Site admin · Sourcegull</title></svelte:head>

<p class="crumbs"><a href="/site-admin/outbound-webhooks">Outbound webhooks</a> /</p>
<div class="page-header">
	<div>
		<h1>New outbound webhook</h1>
		<p class="page-description">
			Sourcegull posts the selected events to this URL as JSON, signed with the secret.
		</p>
	</div>
</div>

{#if exclusive}
	<ManagedNotice kind="outboundWebhooks" />
{:else}
	<OutboundWebhookForm submitLabel="Create" onsubmit={create} />
{/if}

<style>
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
	}
</style>
