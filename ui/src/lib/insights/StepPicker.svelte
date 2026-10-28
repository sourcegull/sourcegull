<!--
	"Granularity: distance between data points" of the search and capture group creation pages:
	a step value and its unit, with the span of data it gives.
-->
<script lang="ts">
	import { STEP_UNITS, stepDescription, stepValueError, type StepUnit } from './creation.ts';

	interface Props {
		stepValue: string;
		stepUnit: StepUnit;
		submitted: boolean;
	}

	let { stepValue = $bindable(), stepUnit = $bindable(), submitted }: Props = $props();

	let touched = $state(false);
	const error = $derived((touched || submitted) && stepValueError(stepValue));
</script>

<fieldset class="step">
	<legend class="label">Granularity: distance between data points</legend>
	<div class="row">
		<input
			class="input value"
			type="number"
			min="1"
			placeholder="ex. 2"
			aria-label="Step between data points"
			aria-invalid={error ? 'true' : undefined}
			aria-describedby="insight-step-hint"
			value={stepValue}
			oninput={(e) => (stepValue = e.currentTarget.value)}
			onblur={() => (touched = true)}
		/>
		{#each STEP_UNITS as u (u.unit)}
			<label class="radio"><input type="radio" name="step" value={u.unit} bind:group={stepUnit} /> {u.label}</label>
		{/each}
	</div>
	{#if error}
		<span class="error" id="insight-step-hint" role="alert">{error}</span>
	{:else}
		<span class="hint muted" id="insight-step-hint">{stepDescription(stepValue, stepUnit)}</span>
	{/if}
</fieldset>

<style>
	.step {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
	}
	legend {
		padding: 0;
		margin-bottom: var(--space-2);
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2) var(--space-3);
	}
	.value {
		width: 6rem;
	}
	.radio {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
	}
	.hint {
		font-size: 0.85rem;
	}
	.error {
		color: var(--color-danger);
		font-size: 0.9em;
	}
</style>
