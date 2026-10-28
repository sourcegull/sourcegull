<script lang="ts">
	import type { CreateCodeMonitorMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import MonitorForm, { type MonitorFields } from '#lib/monitors/MonitorForm.svelte';
	import { CREATE_MONITOR, actionInput, canShareUnfiltered, viewerNamespaces } from '#lib/monitors/queries.ts';

	const user = $derived(page.data.auth?.user ?? null);
	// A `?q=<query>` link (e.g. from a search results page) prefills the trigger.
	const prefill = page.url.searchParams.get('q');

	let namespaces = $state.raw<{ id: string; label: string }[]>([]);
	let canUnfilter = $state(false);
	let ready = $state(false);
	let error = $state<string | null>(null);
	let busy = $state(false);

	onMount(async () => {
		try {
			[namespaces, canUnfilter] = await Promise.all([viewerNamespaces(), canShareUnfiltered()]);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			ready = true;
		}
	});

	async function create(v: MonitorFields): Promise<void> {
		busy = true;
		error = null;
		try {
			const r = await graphql<CreateCodeMonitorMutation, Record<string, unknown>>(
				CREATE_MONITOR,
				{
					monitor: {
						namespace: v.namespace,
						description: v.description,
						enabled: v.enabled,
						unfilteredResults: v.unfiltered
					},
					trigger: { query: v.query },
					actions: v.actions.map(actionInput)
				}
			);
			await goto(r.createCodeMonitor.url);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>New watch · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<p class="crumbs"><a href="/watches">Watches</a> /</p>
	<div class="page-header">
		<div>
			<h1>New watch</h1>
			<p class="page-description">Pick a commit or diff search to watch and how you want to be told about new matches.</p>
		</div>
	</div>
	{#if !ready}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{:else if user}
		<MonitorForm
			initial={{
				namespace: namespaces[0]?.id ?? user.id,
				description: '',
				query: prefill ?? 'type:diff ',
				enabled: true,
				unfiltered: false,
				actions: [
					{
						kind: 'email',
						id: null,
						enabled: true,
						includeResults: false,
						url: '',
						priority: 'NORMAL',
						header: '',
						removeHeader: false,
						recipients: [user.id],
						botToken: '',
						chatId: '',
						masked: ''
					}
				]
			}}
			namespaces={namespaces.length > 0 ? namespaces : [{ id: user.id, label: `@${user.username}` }]}
			submitLabel="Create watch"
			viewer={user.username}
			siteAdmin={user.siteAdmin}
			canShareUnfiltered={canUnfilter}
			{error}
			{busy}
			onsubmit={create}
			oncancel={() => goto('/watches')}
		/>
	{/if}
</div>

