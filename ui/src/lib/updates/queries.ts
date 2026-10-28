/**
 * GraphQL operations of the site admin Updates page: the product
 * version and the database's upgrade readiness (schema drift against the version it is at, and
 * out-of-band migrations that must finish first).
 */
import type { UpgradeReadinessQuery } from '#lib/graphql-types.ts';

export const UPGRADE_READINESS = `query UpgradeReadiness {
	site {
		productVersion
		upgradeReadiness {
			schemaVersion
			schemaDriftNote
			schemaDrift { name problem solution diff statements urlHint }
			requiredOutOfBandMigrations { id description progress deprecated applyReverse }
		}
	}
}`;

export interface SchemaDriftSummary {
	name: string;
	problem: string;
	solution: string;
	diff: string | null;
	statements: string[] | null;
	urlHint: string | null;
}

export type UpgradeReadinessResult = UpgradeReadinessQuery;

/** Sourcegull's own version from `productVersion` (`5.3.0+sourcegull.0.1.0` → `0.1.0`). */
export function sourcegullVersion(productVersion: string): string {
	const i = productVersion.indexOf('+sourcegull.');
	return i < 0 ? productVersion : productVersion.slice(i + '+sourcegull.'.length);
}

/** All fix statements of a drift report, one per line, to copy into psql. */
export function allStatements(drift: SchemaDriftSummary[]): string {
	return drift.flatMap((d) => d.statements ?? []).join('\n');
}
