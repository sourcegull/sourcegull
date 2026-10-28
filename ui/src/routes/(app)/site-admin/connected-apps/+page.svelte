<script lang="ts">
	import ConnectedApps from '#lib/components/ConnectedApps.svelte';
</script>

<svelte:head><title>Connected apps · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Connected apps</h1>
		<p class="page-description">
			MCP clients users allowed to act as them through OAuth sign-in. Revoking ends an app's access at once;
			the user can allow it again. Turn OAuth sign-in for MCP clients off with
			<code class="code-inline">mcp.oauth.enabled</code> in the site configuration.
		</p>
	</div>
</div>

<ConnectedApps all />
