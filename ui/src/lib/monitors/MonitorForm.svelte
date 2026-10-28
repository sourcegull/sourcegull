<script lang="ts" module>
	import type { ActionDraft } from './queries.ts';

	export interface MonitorFields {
		namespace: string;
		description: string;
		query: string;
		enabled: boolean;
		actions: ActionDraft[];
		/** Every email recipient gets all results, found with the access of the user it runs as. */
		unfiltered: boolean;
	}
</script>

<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import QueryInput from '#lib/search/QueryInput.svelte';
	import { parsePatternType } from '#lib/search/query.ts';
	import {
		TEST_EMAIL,
		TEST_MATTERMOST_WEBHOOK,
		TEST_SLACK_WEBHOOK,
		TEST_TELEGRAM,
		TEST_WEBHOOK,
		actionInput,
		actionLabel,
		isMonitorQuery,
		isOrgId
	} from './queries.ts';

	interface Props {
		initial: MonitorFields;
		/** Namespaces the viewer may act for: owners and email recipients. */
		namespaces: { id: string; label: string }[];
		submitLabel: string;
		error?: string | null;
		busy?: boolean;
		onsubmit: (v: MonitorFields) => void;
		oncancel?: () => void;
		/** The viewer's username. */
		viewer: string;
		siteAdmin?: boolean;
		/** Whether the viewer may make the watch send unfiltered results. */
		canShareUnfiltered?: boolean;
		/** The stored watch: whom it runs as, and who made it unfiltered. */
		stored?: { runsAs: string | null; unfilteredBy: string | null; unfilteredAt: string | null } | null;
	}

	let {
		initial,
		namespaces,
		submitLabel,
		error = null,
		busy = false,
		onsubmit,
		oncancel,
		viewer,
		siteAdmin = false,
		canShareUnfiltered = false,
		stored = null
	}: Props = $props();

	// The form edits a copy; `initial` only seeds it.
	// svelte-ignore state_referenced_locally
	let namespace = $state(initial.namespace);
	// svelte-ignore state_referenced_locally
	let description = $state(initial.description);
	// svelte-ignore state_referenced_locally
	let query = $state(initial.query);
	// svelte-ignore state_referenced_locally
	let enabled = $state(initial.enabled);
	// svelte-ignore state_referenced_locally
	let actions = $state<ActionDraft[]>(initial.actions.map((a) => ({ ...a, recipients: [...a.recipients] })));
	// svelte-ignore state_referenced_locally
	let unfiltered = $state(initial.unfiltered);
	let acknowledged = $state(false);

	/** Per-action "send test" status, by index. */
	let testStatus = $state<Record<number, { ok: boolean; message: string } | undefined>>({});
	let testing = $state<number | null>(null);

	const URL_PLACEHOLDER: Partial<Record<ActionDraft['kind'], string>> = {
		webhook: 'https://example.com/hooks/watch',
		slackWebhook: 'https://hooks.slack.com/services/…',
		mattermostWebhook: 'https://mattermost.example.com/hooks/…'
	};
	const URL_HINT: Partial<Record<ActionDraft['kind'], string>> = {
		webhook: 'Receives a JSON POST with the watch, its query and the result count.',
		slackWebhook: 'Slack or Rocket.Chat incoming webhook.',
		mattermostWebhook: 'Mattermost incoming webhook (Integrations › Incoming Webhooks).'
	};

	/** Whom the watch runs as once saved: a user's watch as the user, an organization's as the viewer. */
	const runsAfterSave = $derived(
		isOrgId(namespace) ? viewer : (namespaces.find((n) => n.id === namespace)?.label.replace(/^@/, '') ?? viewer)
	);
	const emailsOrg = $derived(actions.some((a) => a.kind === 'email' && a.recipients.some(isOrgId)));
	/** Unfiltered results someone else turned on, which saving turns off. */
	const othersUnfiltered = $derived(stored?.unfilteredBy && stored.unfilteredBy !== runsAfterSave ? stored : null);
	// Turning it on (not keeping it on) needs the acknowledgement.
	// svelte-ignore state_referenced_locally
	const needsAcknowledgement = $derived(unfiltered && !initial.unfiltered);

	const patternType = $derived(parsePatternType(/patterntype:(\S+)/i.exec(query)?.[1]));
	const typeMissing = $derived(query.trim() !== '' && !isMonitorQuery(query));

	function addAction(kind: ActionDraft['kind']): void {
		actions.push({
			kind,
			id: null,
			enabled: true,
			includeResults: false,
			url: '',
			priority: 'NORMAL',
			header: '',
			removeHeader: false,
			recipients: kind === 'email' && namespace ? [namespace] : [],
			botToken: '',
			chatId: '',
			masked: ''
		});
	}

	function removeAction(i: number): void {
		actions.splice(i, 1);
		testStatus = {};
	}

	function toggleRecipient(a: ActionDraft, id: string, on: boolean): void {
		a.recipients = on ? [...a.recipients, id] : a.recipients.filter((r) => r !== id);
	}

	async function sendTest(i: number): Promise<void> {
		const a = actions[i];
		if (!a) return;
		testing = i;
		testStatus[i] = undefined;
		const vars = { namespace, description: description.trim() || 'Watch' };
		// A stored action's ID makes the server use its stored URL, token or header when the field
		// is empty.
		const stored = { ...vars, action: a.id };
		try {
			const input = actionInput(a);
			if (a.kind === 'email') {
				await graphql(TEST_EMAIL, { ...stored, email: input.email });
			} else if (a.kind === 'webhook') {
				await graphql(TEST_WEBHOOK, { ...stored, webhook: input.webhook });
			} else if (a.kind === 'mattermostWebhook') {
				await graphql(TEST_MATTERMOST_WEBHOOK, { ...stored, mattermostWebhook: input.mattermostWebhook });
			} else if (a.kind === 'telegram') {
				await graphql(TEST_TELEGRAM, { ...stored, telegram: input.telegram });
			} else {
				await graphql(TEST_SLACK_WEBHOOK, { ...stored, slackWebhook: input.slackWebhook });
			}
			testStatus[i] = { ok: true, message: 'Test sent.' };
		} catch (err) {
			testStatus[i] = { ok: false, message: errorMessage(err) };
		} finally {
			testing = null;
		}
	}

	function submit(e?: SubmitEvent): void {
		e?.preventDefault();
		onsubmit({
			namespace,
			description: description.trim(),
			query: query.trim(),
			enabled,
			actions: actions.map((a) => ({ ...a, recipients: [...a.recipients] })),
			unfiltered
		});
	}
</script>

<form class="monitor-form" onsubmit={submit}>
	<div class="field">
		<label for="cm-description">Name</label>
		<input id="cm-description" class="input" required bind:value={description} placeholder="New TODOs in the API" />
	</div>
	{#if namespaces.length > 1}
		<div class="field">
			<label for="cm-owner">Owner</label>
			<select id="cm-owner" class="input owner" bind:value={namespace}>
				{#each namespaces as n (n.id)}
					<option value={n.id}>{n.label}</option>
				{/each}
			</select>
		</div>
	{/if}

	<fieldset class="card section">
		<legend>Trigger</legend>
		<span class="label">When new commits match this search</span>
		<QueryInput bind:value={query} {patternType} label="Watch query" onsubmit={() => submit()} />
		<span class="hint" class:warn={typeMissing}>
			Must contain <code>type:diff</code> (changed lines) or <code>type:commit</code> (commit messages). Only commits
			pushed after the watch is created notify; it runs with your repository permissions.
		</span>
	</fieldset>

	<fieldset class="card section">
		<legend>Actions</legend>
		{#each actions as a, i (i)}
			<div class="action">
				<div class="action-head">
					<strong>{actionLabel(a.kind)}</strong>
					<label class="checkbox"><input type="checkbox" bind:checked={a.enabled} /> Enabled</label>
					<span class="spacer"></span>
					<button type="button" class="btn btn-sm" disabled={testing !== null || busy} onclick={() => sendTest(i)}>
						{testing === i ? 'Sending…' : 'Send test'}
					</button>
					<button type="button" class="btn btn-sm btn-danger" onclick={() => removeAction(i)}>Remove</button>
				</div>
				{#if a.kind === 'email'}
					<div class="field">
						<span class="label">Recipients</span>
						<div class="recipients">
							{#each namespaces as n (n.id)}
								<label class="checkbox">
									<input
										type="checkbox"
										checked={a.recipients.includes(n.id)}
										onchange={(e) => toggleRecipient(a, n.id, e.currentTarget.checked)}
									/>
									{n.label}
								</label>
							{/each}
						</div>
						<span class="hint">Emails go to each recipient's verified primary address (every member of an organization).</span>
					</div>
					<label class="checkbox">
						<input
							type="checkbox"
							checked={a.priority === 'CRITICAL'}
							onchange={(e) => (a.priority = e.currentTarget.checked ? 'CRITICAL' : 'NORMAL')}
						/>
						Critical (marked in the subject)
					</label>
					<div class="field">
						<label for={`cm-header-${i}`}>Header</label>
						<input
							id={`cm-header-${i}`}
							class="input"
							autocomplete="off"
							disabled={a.removeHeader}
							bind:value={a.header}
							placeholder={a.masked || 'Approved: password'}
						/>
						<span class="hint">
							One <code>Name: value</code> header field added to every email, e.g. to approve messages to a moderated
							mailing list.{#if a.masked}
								Leave empty to keep the stored header.{/if}
						</span>
						{#if a.masked}
							<label class="checkbox"><input type="checkbox" bind:checked={a.removeHeader} /> Remove the stored header</label>
						{/if}
					</div>
				{:else if a.kind === 'telegram'}
					<div class="field">
						<label for={`cm-token-${i}`}>Bot token</label>
						<input
							id={`cm-token-${i}`}
							class="input"
							type="password"
							autocomplete="off"
							required={a.id === null}
							bind:value={a.botToken}
							placeholder={a.masked || '123456:ABC-…'}
						/>
						<span class="hint">
							The token BotFather gave the bot.{#if a.masked}
								Leave empty to keep the stored token.{/if}
						</span>
					</div>
					<div class="field">
						<label for={`cm-chat-${i}`}>Chat</label>
						<input id={`cm-chat-${i}`} class="input" required bind:value={a.chatId} placeholder="-1001234567890 or @channel" />
						<span class="hint">
							A numeric chat ID (negative for groups and channels) or a public channel's <code>@username</code>. Add the
							bot to the chat first.
						</span>
					</div>
				{:else}
					<div class="field">
						<label for={`cm-url-${i}`}>{a.kind === 'webhook' ? 'Webhook URL' : 'Incoming webhook URL'}</label>
						<input
							id={`cm-url-${i}`}
							class="input"
							type="url"
							required={a.id === null}
							bind:value={a.url}
							placeholder={a.masked || URL_PLACEHOLDER[a.kind]}
						/>
						<span class="hint">
							{URL_HINT[a.kind]}{#if a.masked}
								Leave empty to keep the stored URL.{/if}
						</span>
					</div>
				{/if}
				<label class="checkbox">
					<input type="checkbox" bind:checked={a.includeResults} /> Include results
				</label>
				{#if testStatus[i]}
					<div class="alert" class:alert-success={testStatus[i]?.ok} class:alert-danger={!testStatus[i]?.ok} role="status">
						{testStatus[i]?.message}
					</div>
				{/if}
			</div>
		{:else}
			<p class="muted">No actions: runs are logged but nobody is notified.</p>
		{/each}
		<div class="row">
			<button type="button" class="btn btn-sm" onclick={() => addAction('email')}>Add email</button>
			<button type="button" class="btn btn-sm" onclick={() => addAction('webhook')}>Add webhook</button>
			<button type="button" class="btn btn-sm" onclick={() => addAction('slackWebhook')}>Add Slack webhook</button>
			<button type="button" class="btn btn-sm" onclick={() => addAction('mattermostWebhook')}>Add Mattermost webhook</button>
			<button type="button" class="btn btn-sm" onclick={() => addAction('telegram')}>Add Telegram</button>
		</div>
	</fieldset>

	<fieldset class="card section">
		<legend>Who sees which results</legend>
		<p class="text">
			The watch searches with the repository permissions of <strong>@{runsAfterSave}</strong>.
			{#if stored?.runsAs && stored.runsAs !== runsAfterSave}
				It runs as <strong>@{stored.runsAs}</strong> now; saving makes it run as you.
			{/if}
		</p>
		{#if unfiltered}
			<p class="text">Every email recipient gets all results that @{runsAfterSave} can read, unfiltered.</p>
		{:else}
			<p class="text">
				Each email recipient gets only the results in repositories they can read, and no email when none are left; the
				email says so. Webhook, Slack, Mattermost and Telegram messages carry every result @{runsAfterSave} can read.
			</p>
		{/if}
		{#if othersUnfiltered}
			<div class="alert alert-danger" role="alert">
				Unfiltered results were turned on by <strong>@{othersUnfiltered.unfilteredBy}</strong>{#if othersUnfiltered.unfilteredAt}
					on {new Date(othersUnfiltered.unfilteredAt).toLocaleString()}{/if}. Saving turns them off unless you turn them
				on yourself.
			</div>
		{/if}
		{#if canShareUnfiltered}
			<label class="checkbox unfiltered-toggle">
				<input type="checkbox" bind:checked={unfiltered} />
				Send unfiltered results to every email recipient
			</label>
			{#if unfiltered}
				<div class="unfiltered-warning" role="alert">
					<p class="warning-title">Warning: this bypasses repository permissions</p>
					<p>
						Every email recipient{#if emailsOrg}, including every member of the organizations you picked,{/if} will get all
						results this watch finds with <strong>your</strong> access, including code in repositories they are not allowed
						to read. Search results and diffs can contain secrets.
					</p>
					{#if !siteAdmin}
						<p>
							<strong>You are responsible for the consequences of sharing this code.</strong> Turn this on only if every
							recipient may see everything you can read in the watched repositories. Your name and the time you turned it on
							are recorded and shown on the watch.
						</p>
					{/if}
					{#if needsAcknowledgement}
						<label class="checkbox">
							<input type="checkbox" required bind:checked={acknowledged} />
							{siteAdmin
								? 'I understand that recipients get results they are not allowed to read.'
								: 'I understand and take responsibility for sharing these results.'}
						</label>
					{/if}
				</div>
			{/if}
		{/if}
	</fieldset>

	<label class="checkbox">
		<input type="checkbox" bind:checked={enabled} /> Enabled
	</label>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	<div class="row">
		<button class="btn btn-primary" type="submit" disabled={busy}>{submitLabel}</button>
		{#if oncancel}<button class="btn" type="button" onclick={oncancel}>Cancel</button>{/if}
	</div>
</form>

<style>
	.monitor-form {
		display: grid;
		gap: var(--space-3);
	}
	.section {
		display: grid;
		gap: var(--space-2);
		padding: var(--space-3) var(--space-4) var(--space-4);
		margin: 0;
		min-width: 0;
	}
	legend {
		font-weight: 600;
		padding: 0 var(--space-1);
	}
	.field {
		display: grid;
		gap: var(--space-1);
		margin: 0;
	}
	.owner {
		width: auto;
	}
	.hint {
		font-size: 0.85rem;
		color: var(--color-text-muted);
	}
	.hint.warn {
		color: var(--color-warning);
	}
	.action {
		display: grid;
		gap: var(--space-2);
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.action-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.recipients {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
	}
	.spacer {
		flex: 1;
	}
	.text {
		margin: 0;
	}
	.unfiltered-toggle {
		color: var(--color-danger);
		font-weight: 600;
	}
	.unfiltered-warning {
		display: grid;
		gap: var(--space-2);
		padding: var(--space-3) var(--space-4);
		border: 2px solid var(--color-danger);
		border-radius: var(--radius);
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.unfiltered-warning p {
		margin: 0;
	}
	.warning-title {
		font-size: 1.15rem;
		font-weight: 700;
		text-transform: uppercase;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
	}
</style>
