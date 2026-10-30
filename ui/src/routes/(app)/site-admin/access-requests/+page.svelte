<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ACCESS_REQUESTS,
		SET_ACCESS_REQUEST_STATUS,
		type AccessRequestNode,
		type AccessRequestsResult,
		type SetAccessRequestStatusResult,
		type SetAccessRequestStatusVariables
	} from '#lib/queries.ts';

	const PAGE_SIZE = 100;

	let requests = $state<AccessRequestNode[]>([]);
	let totalCount = $state(0);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let setupLink = $state<{ email: string; url: string } | null>(null);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			const result = await graphql<AccessRequestsResult, { first: number }>(ACCESS_REQUESTS, {
				first: PAGE_SIZE
			});
			requests = result.accessRequests.nodes;
			totalCount = result.accessRequests.totalCount;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function decide(
		request: AccessRequestNode,
		status: SetAccessRequestStatusVariables['status']
	): Promise<void> {
		pending = request.id;
		error = null;
		try {
			const result = await graphql<SetAccessRequestStatusResult, SetAccessRequestStatusVariables>(
				SET_ACCESS_REQUEST_STATUS,
				{ id: request.id, status }
			);
			const url = result.setAccessRequestStatus.resetUrl;
			// Relative when `externalURL` is not set: the admin copies it, so make it absolute.
			setupLink = url ? { email: request.email, url: new URL(url, location.origin).href } : null;
			await load();
		} catch (err) {
			error = `Could not update the request from ${request.email}: ${errorMessage(err)}`;
		} finally {
			pending = null;
		}
	}

	const formatDate = (iso: string) => new Date(iso).toLocaleDateString();

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Access requests · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Access requests</h1>
		<p class="page-description">
			{totalCount} pending. People who asked for an account from the sign-in page; approving one
			creates the account and gives you a setup link to send them.
		</p>
		<RelatedLinks />
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if setupLink}
	<div class="alert alert-success" role="status">
		Account created. Send this single-use setup link (valid for 24 hours) to {setupLink.email}
		privately:
		<code class="mono setup-link">{setupLink.url}</code>
	</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Name</th>
				<th scope="col">Email</th>
				<th scope="col">Details</th>
				<th scope="col">Requested</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each requests as request (request.id)}
				<tr>
					<td><strong>{request.name}</strong></td>
					<td>{request.email}</td>
					<td class="details">{request.additionalInfo}</td>
					<td>{formatDate(request.createdAt)}</td>
					<td class="actions">
						<button
							class="btn btn-sm btn-primary"
							type="button"
							disabled={pending === request.id}
							onclick={() => decide(request, 'APPROVED')}>Approve</button
						>
						<button
							class="btn btn-sm"
							type="button"
							disabled={pending === request.id}
							onclick={() => decide(request, 'REJECTED')}>Reject</button
						>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="5" class="empty-cell">{loading ? 'Loading…' : 'No pending access requests.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.setup-link {
		display: block;
		margin-top: var(--space-2);
		word-break: break-all;
	}
	.details {
		white-space: pre-wrap;
		max-width: 28rem;
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
</style>
