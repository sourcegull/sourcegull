<script lang="ts">
	import ExecutorSecretsView from '#lib/executors/ExecutorSecretsView.svelte';
</script>

<svelte:head><title>Executor secrets · Site admin · Sourcegull</title></svelte:head>

<ExecutorSecretsView namespace="global" scopes={['CODEINTEL', 'BATCHES']} />
