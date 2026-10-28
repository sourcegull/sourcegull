<script lang="ts">
	interface Props {
		size?: number;
		class?: string;
	}

	let { size = 16, class: className = '' }: Props = $props();
</script>

<!-- GitLab tanuki (simplified), inline so no external asset is needed. Decorative. -->
<svg class="icon {className}" viewBox="0 0 24 24" width={size} height={size} aria-hidden="true" focusable="false">
	<path fill="#E24329" d="M12 21.4 15.7 10H8.3z" />
	<path fill="#FC6D26" d="M12 21.4 8.3 10H3.1z" />
	<path fill="#FCA326" d="M3.1 10 2 13.4c-.1.3 0 .7.3.9l9.7 7.1z" />
	<path fill="#E24329" d="M3.1 10h5.2L6.1 3.1c-.1-.4-.6-.4-.7 0z" />
	<path fill="#FC6D26" d="M12 21.4 15.7 10h5.2z" />
	<path fill="#FCA326" d="m20.9 10 1.1 3.4c.1.3 0 .7-.3.9L12 21.4z" />
	<path fill="#E24329" d="M20.9 10h-5.2l2.2-6.9c.1-.4.6-.4.7 0z" />
</svg>

<style>
	.icon {
		flex: none;
		display: inline-block;
		vertical-align: middle;
	}
</style>
