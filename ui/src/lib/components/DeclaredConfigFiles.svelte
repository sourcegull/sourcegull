<!--
	The configuration-as-code files in use (SOURCEGULL_RESOURCES_FILE, SOURCEGULL_GLOBAL_SETTINGS_FILE): the
	path, what each manages, whether it is exclusive, when it was last applied, and why it could not
	be applied.
-->
<script lang="ts">
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import { kindLabel, sourceVariable, type DeclaredConfigFile } from '#lib/declared-config.ts';

	interface Props {
		files: DeclaredConfigFile[];
	}

	let { files }: Props = $props();
</script>

{#if files.length > 0}
	<section class="card declared" aria-labelledby="declared-title">
		<div class="card-header"><h2 id="declared-title">Configuration files</h2></div>
		<p class="muted">
			The worker applies these files on the server. What they declare is read-only here; change the
			files instead.
		</p>
		<div class="table-scroll">
			<table class="table table-sm">
				<thead>
					<tr>
						<th scope="col">File</th>
						<th scope="col">Manages</th>
						<th scope="col">Last applied</th>
					</tr>
				</thead>
				<tbody>
					{#each files as f (f.source)}
						<tr data-declared-source={f.source}>
							<td>
								<strong>{sourceVariable(f.source)}</strong>
								<div class="mono small path">{f.path}</div>
							</td>
							<td>
								{#if f.source === 'global_settings'}
									global settings
								{:else if f.kinds.length === 0}
									<span class="muted">nothing</span>
								{:else}
									{f.kinds.map(kindLabel).join(', ')}
								{/if}
								{#if f.exclusive}
									<span class="badge badge-info" title="Every object of these kinds comes from the file">exclusive</span>
								{/if}
							</td>
							<td>
								{#if f.appliedAt}
									<span title={formatDate(f.appliedAt)}>{relativeTime(f.appliedAt)}</span>
								{:else}
									<span class="muted">not yet</span>
								{/if}
								<div class="muted small" title={formatDate(f.checkedAt)}>read {relativeTime(f.checkedAt)}</div>
							</td>
						</tr>
						{#if f.error}
							<tr class="failure">
								<td colspan="3">
									<div class="alert alert-danger" role="alert">
										<strong>Could not apply the file{#if f.errorAt}
												({relativeTime(f.errorAt)}){/if}.</strong> What it applied before stays.
										<pre class="error-text">{f.error}</pre>
									</div>
								</td>
							</tr>
						{/if}
					{/each}
				</tbody>
			</table>
		</div>
	</section>
{/if}

<style>
	.declared {
		margin-bottom: var(--space-4);
	}
	.path {
		overflow-wrap: anywhere;
	}
	.failure .alert {
		margin: 0;
	}
	.error-text {
		margin: var(--space-1) 0 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: var(--text-sm);
	}
</style>
