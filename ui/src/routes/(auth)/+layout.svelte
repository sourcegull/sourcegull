<script lang="ts">
	import Wordmark from '#lib/components/Wordmark.svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<main class="auth-layout">
	<div class="brand" aria-hidden="true">
		<img src="/logo.png" alt="" width="75" height="55" />
		<Wordmark />
	</div>
	<div class="card auth-card">
		{@render children()}
	</div>
</main>

<style>
	.auth-layout {
		min-height: 100vh;
		display: flex;
		flex-direction: column;
		align-items: center;
		padding: 12vh var(--space-4) var(--space-8);
		background:
			radial-gradient(60rem 30rem at 50% -10%, var(--color-primary-subtle), transparent 70%),
			var(--color-bg-subtle);
	}
	.brand {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
		letter-spacing: -0.02em;
		margin-bottom: var(--space-6);
	}
	.auth-card {
		width: 100%;
		max-width: 24rem;
		padding: var(--space-8);
		box-shadow: var(--shadow-md);
	}
</style>
