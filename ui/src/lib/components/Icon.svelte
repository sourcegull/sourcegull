<script lang="ts" module>
	/**
	 * Stroke icons on a 24px grid (drawn for Sourcegull). Each entry is the inner SVG markup; all use
	 * `currentColor`, so an icon takes the text colour of its parent.
	 */
	const ICONS = {
		search: '<circle cx="11" cy="11" r="6.5"/><path d="m16 16 4.5 4.5"/>',
		file: '<path d="M13.5 3H7a1.5 1.5 0 0 0-1.5 1.5v15A1.5 1.5 0 0 0 7 21h10a1.5 1.5 0 0 0 1.5-1.5V8z"/><path d="M13.5 3v5h5"/>',
		folder: '<path d="M3.5 6.5A1.5 1.5 0 0 1 5 5h4.5l2 2.5H19a1.5 1.5 0 0 1 1.5 1.5v8.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/>',
		filter: '<path d="M4 5.5h16l-6.25 7.5v5.5l-3.5 1.5v-7z"/>',
		commit: '<circle cx="12" cy="12" r="3.5"/><path d="M3 12h5.5M15.5 12H21"/>',
		branch: '<circle cx="7" cy="5.5" r="2"/><circle cx="7" cy="18.5" r="2"/><circle cx="17" cy="7.5" r="2"/><path d="M7 7.5v9M17 9.5c0 4-5 3.5-9 7"/>',
		clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
		code: '<path d="m8.5 7-5 5 5 5M15.5 7l5 5-5 5"/>',
		symbol: '<path d="M12 3.5 19.5 8v8L12 20.5 4.5 16V8z"/><path d="M4.5 8 12 12.5 19.5 8M12 12.5v8"/>',
		link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
		repo: '<path d="M5 4.5A1.5 1.5 0 0 1 6.5 3H19v15H6.5A1.5 1.5 0 0 0 5 19.5z"/><path d="M5 19.5A1.5 1.5 0 0 0 6.5 21H19v-3"/><path d="M9 7h6"/>',
		notebook: '<rect x="5" y="3" width="14" height="18" rx="2"/><path d="M9 3v18M12.5 8H16M12.5 11.5H16"/>',
		bookmark: '<path d="M6.5 3.5h11v17L12 16.5l-5.5 4z"/>',
		monitor: '<path d="M6 16V11a6 6 0 1 1 12 0v5l1.5 2h-15z"/><path d="M10 20.5a2 2 0 0 0 4 0"/>',
		insights: '<path d="M4 20h16"/><path d="M7 16v-4M12 16V7M17 16v-7"/>',
		batch: '<path d="m12 3 8.5 4.5L12 12 3.5 7.5z"/><path d="m3.5 12 8.5 4.5 8.5-4.5"/><path d="m3.5 16.5 8.5 4.5 8.5-4.5"/>',
		context: '<circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="4.5"/><circle cx="12" cy="12" r="0.75"/>',
		jobs: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
		compute: '<path d="m5 7 4.5 5L5 17"/><path d="M12 17h7"/>',
		users: '<circle cx="9" cy="8.5" r="3.5"/><path d="M3 20a6 6 0 0 1 12 0"/><path d="M15.5 5.2a3.5 3.5 0 0 1 0 6.6M17.5 14.5A6 6 0 0 1 21 20"/>',
		admin: '<path d="M12 3 4.5 6v5.5c0 4.5 3.2 8 7.5 9.5 4.3-1.5 7.5-5 7.5-9.5V6z"/><path d="m9 12 2 2 4-4"/>',
		help: '<circle cx="12" cy="12" r="8.5"/><path d="M9.6 9.5a2.5 2.5 0 0 1 4.8.8c0 1.7-2.4 2.2-2.4 3.7"/><path d="M12 17v.01"/>',
		menu: '<path d="M4 7h16M4 12h16M4 17h16"/>',
		close: '<path d="M6 6l12 12M18 6 6 18"/>',
		sidebar: '<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M9.5 4.5v15"/>',
		sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M5.3 18.7l1.4-1.4M17.3 6.7l1.4-1.4"/>',
		moon: '<path d="M19.5 14.5A8 8 0 0 1 9.5 4.5a8 8 0 1 0 10 10z"/>',
		system: '<rect x="3" y="4.5" width="18" height="12" rx="2"/><path d="M8.5 20h7M12 16.5V20"/>',
		key: '<circle cx="8" cy="15" r="4"/><path d="m11 12 8.5-8.5M16 7l2.5 2.5M14 9l2 2"/>',
		org: '<rect x="4" y="3.5" width="11" height="17" rx="1.5"/><path d="M15 9h4.5a.5.5 0 0 1 .5.5v11H15M7.5 7.5h4M7.5 11h4M7.5 14.5h4"/>',
		signout: '<path d="M14 4.5H6.5a1.5 1.5 0 0 0-1.5 1.5v12a1.5 1.5 0 0 0 1.5 1.5H14"/><path d="M10 12h10.5M17 8.5l3.5 3.5-3.5 3.5"/>',
		settings: '<circle cx="12" cy="12" r="3"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1"/>',
		'chevron-down': '<path d="m6.5 9.5 5.5 5.5 5.5-5.5"/>',
		'chevron-left': '<path d="M14.5 6.5 9 12l5.5 5.5"/>',
		'chevron-right': '<path d="m9.5 6.5 5.5 5.5-5.5 5.5"/>',
		check: '<path d="m5 12.5 4.5 4.5L19 7.5"/>',
		plus: '<path d="M12 5v14M5 12h14"/>',
		star: '<path d="m12 3.5 2.6 5.4 5.9.8-4.3 4.1 1 5.8L12 16.8l-5.2 2.8 1-5.8-4.3-4.1 5.9-.8z"/>',
		upload: '<path d="M12 15V4.5M7.5 9 12 4.5 16.5 9"/><path d="M4.5 15v3.5A1.5 1.5 0 0 0 6 20h12a1.5 1.5 0 0 0 1.5-1.5V15"/>',
		agent: '<rect x="4.5" y="8" width="15" height="11" rx="2.5"/><path d="M12 8V4.5M10.5 4.5h3M2.5 12.5v2.5M21.5 12.5v2.5"/><circle cx="9" cy="13" r="1"/><circle cx="15" cy="13" r="1"/><path d="M10 16.5h4"/>',
		lock: '<rect x="5" y="10.5" width="14" height="10" rx="2"/><path d="M8.5 10.5V7.5a3.5 3.5 0 0 1 7 0v3"/>'
	} as const;

	export type IconName = keyof typeof ICONS;
</script>

<script lang="ts">
	interface Props {
		name: IconName;
		size?: number;
		/** Accessible name; without it the icon is decorative (aria-hidden). */
		label?: string;
		class?: string;
	}

	let { name, size = 16, label, class: className = '' }: Props = $props();
</script>

<svg
	class="icon {className}"
	viewBox="0 0 24 24"
	width={size}
	height={size}
	fill="none"
	stroke="currentColor"
	stroke-width="1.75"
	stroke-linecap="round"
	stroke-linejoin="round"
	role={label ? 'img' : undefined}
	aria-label={label}
	aria-hidden={label ? undefined : 'true'}
	focusable="false"
>
	{@html ICONS[name]}
</svg>

<style>
	.icon {
		flex: none;
		display: inline-block;
		vertical-align: middle;
	}
</style>
