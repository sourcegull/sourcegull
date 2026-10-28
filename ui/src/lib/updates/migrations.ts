/**
 * GraphQL operations of the site admin Migrations page: out-of-band
 * migrations (long-running data migrations such as re-encryption after a key rotation)
 * with their progress and errors, and the direction they run in.
 */
import type { OutOfBandMigrationsQuery } from '#lib/graphql-types.ts';

export const OUT_OF_BAND_MIGRATIONS = `query OutOfBandMigrations {
	outOfBandMigrations {
		id
		team
		component
		description
		introduced
		deprecated
		progress
		created
		lastUpdated
		nonDestructive
		applyReverse
		errors { message created }
	}
}`;

export const SET_MIGRATION_DIRECTION = `mutation SetMigrationDirection($id: ID!, $applyReverse: Boolean!) {
	setMigrationDirection(id: $id, applyReverse: $applyReverse) { alwaysNil }
}`;

export interface OutOfBandMigrationError {
	message: string;
	created: string;
}

export interface OutOfBandMigration {
	id: string;
	team: string;
	component: string;
	description: string;
	introduced: string;
	deprecated: string | null;
	progress: number;
	created: string;
	lastUpdated: string | null;
	nonDestructive: boolean;
	applyReverse: boolean;
	errors: OutOfBandMigrationError[];
}

export type OutOfBandMigrationsResult = OutOfBandMigrationsQuery;

/** Progress as a whole percentage, rounded down so 99.9% never shows as done. */
export function percent(progress: number): number {
	return Math.floor(Math.min(1, Math.max(0, progress)) * 100);
}

/** The migration's state in words. */
export function migrationState(m: Pick<OutOfBandMigration, 'progress' | 'applyReverse'>): string {
	if (m.applyReverse) return m.progress <= 0 ? 'Reverted' : 'Reverting';
	return m.progress >= 1 ? 'Complete' : 'Running';
}
