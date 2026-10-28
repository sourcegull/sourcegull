<script lang="ts">
	import CodeHostLogo from '#lib/components/CodeHostLogo.svelte';
	import { EVENTS, logEvent } from '#lib/events.ts';
	import { codeHostName, codeHostURL, type CodeHostTarget, type ExternalLink } from './codehost.ts';

	interface Props {
		/** The repo's external link; nothing is rendered without one. */
		link: ExternalLink | null | undefined;
		target: CodeHostTarget;
		/** Show "View on GitLab" (or the repo's code host) next to the icon. */
		label?: boolean;
		class?: string;
	}

	let { link, target, label = false, class: className = '' }: Props = $props();

	const name = $derived(link ? codeHostName(link) : '');
	const title = $derived(
		target.kind === 'blob'
			? `View file on ${name}`
			: target.kind === 'commit'
				? `View commit on ${name}`
				: `View on ${name}`
	);

	/** A click (or middle click) that opens the code host. */
	function opened(e: MouseEvent): void {
		if (e.type === 'click' || e.button === 1) logEvent(EVENTS.goToCodeHost, { target: target.kind });
	}
</script>

{#if link}
	<a
		class="code-host-link {className || 'plain'}"
		href={codeHostURL(link, target)}
		target="_blank"
		rel="noopener noreferrer"
		{title}
		aria-label={label ? undefined : title}
		onclick={opened}
		onauxclick={opened}
	>
		{#if link.serviceKind === 'GITLAB' || link.serviceKind === 'GITHUB' || link.serviceKind === 'GITEA'}
			<CodeHostLogo kind={link.serviceKind} />
		{:else}
			<svg class="icon" viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
				<path
					fill="none"
					stroke="currentColor"
					stroke-width="2"
					stroke-linecap="round"
					stroke-linejoin="round"
					d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"
				/>
			</svg>
		{/if}
		{#if label}<span>View on {name}</span>{/if}
	</a>
{/if}

<style>
	.code-host-link {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		align-self: center;
		line-height: 1;
	}
	.plain {
		color: var(--color-text-muted);
	}
	.plain:hover {
		color: var(--color-text);
	}
	.icon {
		flex: none;
	}
</style>
