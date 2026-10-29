<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		SITE_USERS,
		lastActiveFilter,
		relativeTime,
		type ActivityFilter,
		type SiteUser,
		type SiteUsersResult,
		type UserOrder
	} from '#lib/analytics/queries.ts';

	// User activity: every live user with their last activity and usage event count,
	// from the local event logs.

	const PAGE = 50;

	let query = $state('');
	let activity = $state<ActivityFilter>('all');
	let order = $state<UserOrder>('LAST_ACTIVE_AT');
	let descending = $state(true);
	let rows = $state<SiteUser[]>([]);
	let totalCount = $state(0);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(offset: number): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<SiteUsersResult, Record<string, unknown>>(SITE_USERS, {
				query: query.trim() || null,
				lastActiveAt: lastActiveFilter(activity),
				limit: PAGE,
				offset,
				orderBy: order,
				descending
			});
			rows = offset ? [...rows, ...r.site.users.nodes] : r.site.users.nodes;
			totalCount = r.site.users.totalCount;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	let timer: ReturnType<typeof setTimeout> | undefined;
	$effect(() => {
		// Reload on any filter change; typing is debounced.
		void [query, activity, order, descending];
		clearTimeout(timer);
		timer = setTimeout(() => void load(0), 200);
		return () => clearTimeout(timer);
	});

	function sortBy(o: UserOrder): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			descending = o !== 'USERNAME';
		}
	}

	const arrow = (o: UserOrder) => (order === o ? (descending ? ' ↓' : ' ↑') : '');
	const ariaSort = (o: UserOrder) => (order === o ? (descending ? 'descending' : 'ascending') : 'none');
</script>

<svelte:head><title>User activity · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>User activity</h1>
		<p class="page-description">
			Last activity and usage events per user, from this instance's event logs. Event counts update
			every half hour.
		</p>
	</div>
	<div class="page-actions">
		<a class="btn btn-secondary" href="/site-admin/analytics">Analytics</a>
	</div>
</div>

<div class="toolbar">
	<input class="input" type="search" placeholder="Search users" aria-label="Search users" bind:value={query} />
	<label>
		<span class="muted">Activity</span>
		<select class="input" bind:value={activity}>
			<option value="all">All users</option>
			<option value="active-week">Active in the last 7 days</option>
			<option value="active-month">Active in the last 30 days</option>
			<option value="inactive-month">Not active in the last 30 days</option>
			<option value="never">Never active</option>
		</select>
	</label>
	<span class="muted small count">{totalCount} users</span>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered table-sm" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col" aria-sort={ariaSort('USERNAME')}><button type="button" class="link" onclick={() => sortBy('USERNAME')}>User{arrow('USERNAME')}</button></th>
				<th scope="col" aria-sort={ariaSort('EMAIL')}><button type="button" class="link" onclick={() => sortBy('EMAIL')}>Email{arrow('EMAIL')}</button></th>
				<th scope="col" aria-sort={ariaSort('LAST_ACTIVE_AT')}><button type="button" class="link" onclick={() => sortBy('LAST_ACTIVE_AT')}>Last active{arrow('LAST_ACTIVE_AT')}</button></th>
				<th scope="col" class="num" aria-sort={ariaSort('EVENTS_COUNT')}><button type="button" class="link" onclick={() => sortBy('EVENTS_COUNT')}>Events{arrow('EVENTS_COUNT')}</button></th>
				<th scope="col" aria-sort={ariaSort('CREATED_AT')}><button type="button" class="link" onclick={() => sortBy('CREATED_AT')}>Created{arrow('CREATED_AT')}</button></th>
			</tr>
		</thead>
		<tbody>
			{#each rows as u (u.id)}
				<tr>
					<td>
						<a href="/site-admin/users?query={encodeURIComponent(u.username)}">{u.username}</a>
						{#if u.displayName}<span class="muted">{u.displayName}</span>{/if}
						{#if u.siteAdmin}<span class="badge badge-info">admin</span>{/if}
						{#if u.locked}<span class="badge badge-warning">locked</span>{/if}
					</td>
					<td>{u.email ?? '–'}</td>
					<td title={u.lastActiveAt ? new Date(u.lastActiveAt).toLocaleString() : undefined}>{relativeTime(u.lastActiveAt)}</td>
					<td class="num">{u.eventsCount}</td>
					<td>{new Date(u.createdAt).toLocaleDateString()}</td>
				</tr>
			{:else}
				<tr><td colspan="5" class="empty-cell">{loading ? 'Loading…' : 'No users match.'}</td></tr>
			{/each}
		</tbody>
	</table>
</div>
{#if rows.length < totalCount}
	<div class="more">
		<button class="btn" type="button" disabled={loading} onclick={() => load(rows.length)}>Load more</button>
	</div>
{/if}

<style>
	.toolbar {
		margin-bottom: var(--space-4);
		gap: var(--space-3);
	}
	.toolbar input[type='search'] {
		max-width: 20rem;
	}
	.toolbar label {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.count {
		margin-left: auto;
	}
	button.link {
		background: none;
		border: none;
		padding: 0;
		font: inherit;
		text-transform: inherit;
		letter-spacing: inherit;
		color: inherit;
		cursor: pointer;
	}
	button.link:hover {
		color: var(--color-text);
	}
	td .badge,
	td .muted {
		margin-left: var(--space-1);
	}
	.more {
		display: flex;
		justify-content: center;
		margin-top: var(--space-4);
	}
</style>
