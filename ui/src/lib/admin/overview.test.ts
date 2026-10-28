import { describe, expect, it } from 'vitest';
import { SITE_ADMIN_HEALTH, diskUsedPercent } from './overview.ts';

describe('site admin overview', () => {
	it('asks for every status message kind', () => {
		expect(SITE_ADMIN_HEALTH).toContain('... on CloningProgress');
		expect(SITE_ADMIN_HEALTH).toContain('... on GitserverDiskThresholdReached');
	});
	it('computes the used share of a disk', () => {
		expect(diskUsedPercent({ freeDiskSpaceBytes: '25', totalDiskSpaceBytes: '100' })).toBe(75);
		expect(diskUsedPercent({ freeDiskSpaceBytes: '0', totalDiskSpaceBytes: '0' })).toBeNull();
	});
});
