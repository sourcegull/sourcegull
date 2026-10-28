# Sourcegull UI design rules

The look we aim for is a calm developer tool: neutral surfaces, one accent colour (indigo), crisp
1px borders, small shadows, a clear type hierarchy and comfortable density. Everything below lives
in `src/app.css` (tokens and global classes) and the app shell in `src/routes/(app)/+layout.svelte`.

**Rule of thumb for page work:** use the global classes first, and tokens (`var(--…)`) for
anything scoped. Never hard-code a colour, font size, radius or shadow in a page; if a token is
missing, add it to `app.css` (in both themes) and list it here.

## Tokens

| Group | Tokens | Notes |
|---|---|---|
| Surfaces | `--color-bg`, `--color-bg-subtle`, `--color-bg-muted`, `--color-surface`, `--color-surface-raised`, `--color-sidebar` | `surface` for cards and inputs, `surface-raised` for menus/popovers |
| Interaction | `--color-bg-hover`, `--color-bg-active` | hover / pressed or selected backgrounds of rows, menu items |
| Borders | `--color-border`, `--color-border-strong` | `border` for frames and dividers, `strong` for controls |
| Text | `--color-text`, `--color-text-muted`, `--color-text-subtle` | muted for secondary text, subtle for placeholders and group labels |
| Accent | `--color-primary`, `--color-primary-hover`, `--color-primary-text`, `--color-primary-subtle`, `--color-link`, `--color-focus` | `primary-subtle` is the selected-item background |
| Status | `--color-{danger,warning,success}`, `-bg`, `-border`; `--color-info`, `--color-info-bg`, `--color-info-border` | text / tinted background / border of the same status |
| Palettes | `--gray-0 … --gray-950`, `--accent-50 … --accent-700` | raw scales; prefer the semantic tokens, which change with the theme |
| Type | `--font-sans`, `--font-mono`; `--text-xs` 12, `--text-sm` 13, `--text-md` 14 (body), `--text-lg` 16, `--text-xl` 20, `--text-2xl` 24; `--weight-{regular,medium,semibold}` | h1 = 2xl, h2 = xl, h3 = lg. Use medium (500) for UI labels, semibold (600) for headings; avoid 700 |
| Spacing | `--space-1` 4px, `-2` 8, `-3` 12, `-4` 16, `-5` 20, `-6` 24, `-8` 32, `-10` 40, `-12` 48 | 4px grid |
| Radii | `--radius-sm` 4, `--radius` 6 (controls), `--radius-lg` 10 (cards, menus), `--radius-full` | |
| Elevation | `--shadow-xs` (controls, cards), `--shadow-sm` (hovered card), `--shadow-md`, `--shadow-lg` (menus, popovers, drawer); `--overlay` (modal backdrop) | |
| Focus | `--focus-ring` (box-shadow) | inputs and buttons use it; everything else gets the global 2px `:focus-visible` outline |
| Layout | `--nav-height` (top bar, use for sticky offsets), `--sidebar-width`, `--sidebar-rail-width`, `--content-max` (80rem), `--control-height{,-sm,-lg}` | |
| Motion | `--duration` 150ms, `--ease` | transitions only on colour, border, shadow, transform; reduced motion disables them globally |
| Code/charts | `--syntax-*`, `--editor-*`, `--diff-*` (`-added-bg`/`-deleted-bg` rows, `-added-text`/`-deleted-text` for +/- counts and markers), `--match-bg`/`--match-ring` (search match highlight), `--series-*`, `--chart-grid` | Syntax colours come only from these tokens (`.hl-*` classes in `app.css`, CodeMirror themes), so both themes work |

Fonts: Inter / JetBrains Mono are used when installed locally, otherwise the system UI and
monospace stacks. No web fonts are bundled (the CSP allows only `font-src 'self'`; bundle via a
dependency later if wanted).

## Themes

Light by default; dark follows `prefers-color-scheme`; the user menu's Theme switch
(System / Light / Dark) sets `<html data-theme>` and stores it under `localStorage['sourcegull.theme']`
(`src/lib/theme/theme.ts`; `app.html` applies it before first paint). The dark values appear twice
in `app.css` (media query and `[data-theme='dark']`): edit both. Check every page in both themes.

## Components (global classes)

- **Buttons** `.btn` + variant `.btn-primary` (one per view, the main action), `.btn-secondary`
  (filled gray), `.btn-ghost` (toolbar/icon buttons), `.btn-danger` (outlined destructive),
  `.btn-primary.btn-danger` or `.btn-danger-solid` (confirming a destructive action),
  `.btn-link`. Sizes `.btn-sm`, `.btn-lg`; `.btn-icon` for square icon buttons (give them an
  `aria-label`); `.btn-block`; `.btn-group` joins buttons. Segmented filters: `.btn.btn-sm` with
  `class:btn-primary` on the selected one, inside a `.btn-group` (`role="group"`, `aria-pressed`
  on each button, counts in `<span class="count">`); the group wraps on narrow screens.
- **Forms** `.field` (label + control + `.hint`), `.label`, `.input` (also on `<select>` and
  `<textarea>`; `.select`, `.textarea` are aliases), `.input-sm`, `.checkbox`. Mark errors with
  `aria-invalid="true"`.
- **Cards** `.card`, optional `.card-header` (title + actions, full-bleed divider),
  `.card-interactive` for clickable cards. `.popover-surface` for menus and popovers.
- **Tables** `.table`, wrapped in `.table-scroll` when it can be wide; `.table-bordered` for a
  framed table, `.table-sm` for dense admin tables. Header cells are small caps-style (uppercase
  12px, muted, subtle background). Numeric columns get `.num` on the `<th>` and the `<td>`s
  (right-aligned, tabular figures). The single "No items" row is `<td colspan=… class="empty-cell">`
  (centred, muted; keep it a `<td>`, e2e checks wait on its text).
- **Badges** `.badge` (neutral), `.badge-primary`/`.badge-info`, `.badge-success`,
  `.badge-warning`, `.badge-danger`.
- **Alerts** `.alert` + `.alert-{info,success,warning,danger}`: tinted background with a 3px
  status-coloured left edge. Use `role="alert"` for errors.
- **Tabs** `<nav class="tabs">` of links with `aria-current="page"`, or `.tab` buttons with
  `role="tab"` / `aria-selected`.
- **Page header** `.page-header` holding a `<div>` with the `<h1>` and an optional
  `<p class="page-description">` saying what the page is for, and a `<div class="page-actions">`
  with buttons. Blocks inside a page are titled with `.section-header` (an `<h2>` plus an optional
  note or actions on the right).
- **List group** `.list-group` (a `<ul>` framed like a card, rows divided; rows with a title link
  get a hover background) with `.list-item-title` (link + badges; `.push` moves trailing actions
  right), `.list-item-meta` (muted small line), `.list-item-description`; `li.empty` for the
  "nothing matches" row. Use it for notebooks, saved searches, monitors, search jobs.
- **Filter bar** `.filter-bar` above a list: search input grows, selects and `.btn-group`
  segmented controls keep their size; put the count last with `margin-left: auto`.
- **Segmented control** `.btn-group` of `.btn`s, the selected one with `aria-pressed="true"`
  (toggles) or `aria-selected="true"` (`role="tab"`).
- **Breadcrumbs** `<p class="crumbs">` (or `<nav class="crumbs">`) above the page header.
- **Empty state** `.empty-state` (dashed frame, centred; optional `.icon`, `<h3>`, `<p>`, a button).
  The bare `.empty` class is used by lists for "No items" rows and is scoped per component.
- **Loading** `.spinner` (inherits colour, 1em), `.loading` (spinner + muted text row),
  `.skeleton` (shimmer block; set width/height inline).
- **Text** `.muted`, `.subtle`, `.small`, `.mono`, `.error-text`, `.code-inline` (chip around
  inline code), `<kbd>`, `.visually-hidden`.
- **Queries** `#lib/search/QueryText.svelte` shows a query with the search box's syntax colours (example chips, recent searches); `.query-chip` on the search page frames it as a clickable chip.
- **Icons** `#lib/components/Icon.svelte`: `<Icon name="search" size={16} />`, 24px-grid stroke
  icons in `currentColor`; decorative unless `label` is passed. Add new icons to its `ICONS` map
  in the same style (stroke 1.75, round caps).

## Layout

- **App shell** (`(app)/+layout.svelte`): left sidebar (`AppSidebar.svelte`, items from
  `src/lib/components/nav.ts`, grouped Code / Automate / Admin) + a sticky top bar of
  `--nav-height` with the code search box (hidden on `/search`), the site-admin status indicator
  and the user menu. The sidebar collapses to an icon rail (stored in
  `localStorage['sourcegull.sidebar.collapsed']`); below 900px it becomes a drawer opened with the
  hamburger (Escape and the backdrop close it, focus returns to the hamburger). New top-level
  sections are added in `nav.ts`, not in the layout.
- **Page body**: wrap a page in `.page` (centred, max `--content-max`, 24px gutters, 16px on
  phones); forms and single-item pages add `.page-narrow` (48rem). Section pages with their own navigation (site admin, settings) use `SidebarLayout`,
  which is the page frame itself (same width and gutters as `.page`), so pages inside it start
  with their `.page-header` and no `.page` wrapper. It takes `items` (flat) or `sections`
  (labelled groups, `src/lib/components/side-nav.ts`); the current item is the longest matching
  href (`exact: true` for an index page). Below 900px the side navigation collapses into a
  disclosure button showing the current page. The site admin groups live in
  `src/lib/admin/nav.ts`, which the `/site-admin` overview's section cards also read.
  Full-height views (repository/file view) subtract `--nav-height` from `100vh`.
- **Rhythm**: 24px between page header and content, 16px between blocks, 8px inside groups of
  controls. Forms are a single column, max ~40rem wide.
- **Responsive**: no horizontal page scroll down to 360px; wide tables scroll inside
  `.table-scroll`; toolbars and page headers wrap.
- **Accessibility**: every interactive element is reachable by keyboard and has a visible focus
  style; icon-only buttons need `aria-label`; the current nav item has `aria-current="page"`;
  colour is never the only signal (pair with text or an icon). Text contrast >= 4.5:1 in both
  themes.
