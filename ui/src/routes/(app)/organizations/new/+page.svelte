<script lang="ts">
	import { goto } from '$app/navigation';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_ORGANIZATION,
		type CreateOrganizationResult,
		type Vars
	} from '#lib/orgs/queries.ts';

	let name = $state('');
	let displayName = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);

	async function create(): Promise<void> {
		busy = true;
		error = null;
		try {
			const result = await graphql<CreateOrganizationResult, Vars>(CREATE_ORGANIZATION, {
				name: name.trim(),
				displayName: displayName.trim() || null
			});
			await goto(`${result.createOrganization.url}/members`);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>New organization · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<div class="page-header">
		<div>
			<h1>New organization</h1>
			<p class="page-description">
				An organization groups users: its members share settings and can own bulk changes, saved searches, notebooks and
				watches. You become its first member.
			</p>
		</div>
	</div>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	<form
		class="card"
		onsubmit={(event) => {
			event.preventDefault();
			void create();
		}}
	>
		<div class="field">
			<label class="label" for="name">Name</label>
			<input id="name" class="input" bind:value={name} required placeholder="acme" autocomplete="off" />
			<p class="hint">Letters, digits, <code>-</code> and <code>.</code>; used in URLs.</p>
		</div>
		<div class="field">
			<label class="label" for="display-name">Display name</label>
			<input id="display-name" class="input" bind:value={displayName} placeholder="Acme Corporation" />
		</div>
		<button class="btn btn-primary" type="submit" disabled={busy || !name.trim()}>Create organization</button>
	</form>
</div>
