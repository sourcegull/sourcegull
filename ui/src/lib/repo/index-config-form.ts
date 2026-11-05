/**
 * The form of a repository's index configuration (`{"index_jobs": [...]}`, read by
 * `crates/autoindex/src/config.rs`). The server has no schema for it, so it is written here in
 * the shape the settings forms read.
 */
import { buildField, type FormField, type JsonSchema } from '#lib/site-config-form.ts';

const lines = (description: string): JsonSchema => ({
	type: 'array',
	description,
	items: { type: 'string' },
	'x-whole-items': true
});

const SCHEMA: JsonSchema = {
	definitions: {
		Step: {
			type: 'object',
			required: ['image'],
			properties: {
				image: { type: 'string', description: 'The container image of the step.' },
				root: {
					type: 'string',
					description: 'Directory the step runs in, relative to the repository root. Empty for the root.'
				},
				commands: lines('Commands the step runs, in order.')
			}
		},
		Job: {
			type: 'object',
			required: ['indexer'],
			properties: {
				indexer: {
					type: 'string',
					description: "The indexer's container image, for example `sourcegraph/scip-go`."
				},
				root: {
					type: 'string',
					description: 'Directory the indexer runs in, relative to the repository root. Empty for the root.'
				},
				indexer_args: lines("The indexer's command line, one argument per entry."),
				outfile: { type: 'string', description: 'The index file the indexer writes, relative to the root above.' },
				local_steps: lines("Commands run in the indexer's container before the indexer."),
				steps: {
					type: 'array',
					description: 'Steps run before the indexer, each in its own container.',
					items: { $ref: '#/definitions/Step' }
				},
				requestedEnvVars: {
					type: 'array',
					description: 'Names of executor secrets passed to the steps as environment variables.',
					items: { type: 'string' }
				}
			}
		}
	},
	properties: {
		index_jobs: {
			type: 'array',
			description: 'The jobs to run for each commit that is indexed.',
			items: { $ref: '#/definitions/Job' }
		}
	}
};

/** The field that edits the configuration's jobs. */
export function indexJobsField(): FormField {
	return buildField(SCHEMA, 'index_jobs', SCHEMA.properties?.index_jobs ?? {}, ['index_jobs']);
}
