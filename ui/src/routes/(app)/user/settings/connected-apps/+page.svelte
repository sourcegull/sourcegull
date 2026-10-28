<script lang="ts">
	import ConnectedApps from '#lib/components/ConnectedApps.svelte';
</script>

<svelte:head><title>Connected apps · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Connected apps</h1>
		<p class="page-description">
			AI agents and other MCP clients you allowed to use Sourcegull as you, by signing in from the app. They see
			the repositories you can, through the MCP server only. Revoking ends their access at once.
		</p>
	</div>
</div>

<ConnectedApps />
