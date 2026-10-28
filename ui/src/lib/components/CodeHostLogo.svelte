<!-- The logo of a code host connection kind (`GITLAB`, `GITHUB`, `GITEA`); nothing for other kinds. -->
<script lang="ts">
	import GiteaLogo from './GiteaLogo.svelte';
	import GitHubLogo from './GitHubLogo.svelte';
	import GitLabLogo from './GitLabLogo.svelte';

	interface Props {
		kind: string | null | undefined;
		size?: number;
		class?: string;
	}

	let { kind, size = 16, class: className = '' }: Props = $props();
</script>

{#if kind === 'GITLAB'}
	<GitLabLogo {size} class={className} />
{:else if kind === 'GITHUB'}
	<GitHubLogo {size} class={className} />
{:else if kind === 'GITEA'}
	<GiteaLogo {size} class={className} />
{/if}
