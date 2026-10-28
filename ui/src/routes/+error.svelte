<script lang="ts">
	import { page } from '$app/state';
</script>

<svelte:head><title>Error · Sourcegull</title></svelte:head>

<main class="error-page">
	<div class="card">
		<h1>{page.status === 404 ? 'Page not found' : page.status === 403 ? 'Access denied' : 'Something went wrong'}</h1>
		<p class="muted">{page.error?.message ?? 'Unknown error'}</p>
		<div class="toolbar">
			<a class="btn" href="/search">Go to search</a>
			<button class="btn" type="button" onclick={() => location.reload()}>Reload</button>
		</div>
	</div>
</main>

<style>
	.error-page {
		max-width: 36rem;
		margin: 10vh auto;
		padding: 0 var(--space-4);
	}
</style>
