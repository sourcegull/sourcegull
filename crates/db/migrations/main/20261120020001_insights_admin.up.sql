-- Code Insights (docs/PLAN.md §6.15), the remaining API: grouping by commit date
-- (`GroupByField.DATE`, commit and diff searches) and an admin-set backfill priority (the
-- admin backfill queue's "move to front/back of queue").

ALTER TABLE insight_series DROP CONSTRAINT insight_series_group_by_check;
ALTER TABLE insight_series ADD CONSTRAINT insight_series_group_by_check
    CHECK (group_by IN ('repo', 'lang', 'path', 'author', 'date'));

-- The priority of the series' backfill jobs, set by an admin; NULL is the default (the
-- series' creation time in seconds, so newer series go first).
ALTER TABLE insight_series ADD COLUMN backfill_priority bigint;
