<script lang="ts">
	import ExecutorSecretsView from '#lib/executors/ExecutorSecretsView.svelte';
</script>

<svelte:head><title>Executor secrets · Sourcegull</title></svelte:head>

<!-- Auto-indexing only uses global secrets; a user's secrets are for bulk changes. -->
<ExecutorSecretsView namespace="user" scopes={['BATCHES']} />
