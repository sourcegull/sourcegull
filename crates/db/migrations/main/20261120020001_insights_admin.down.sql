ALTER TABLE insight_series DROP COLUMN IF EXISTS backfill_priority;
DELETE FROM insight_series WHERE group_by = 'date';
ALTER TABLE insight_series DROP CONSTRAINT insight_series_group_by_check;
ALTER TABLE insight_series ADD CONSTRAINT insight_series_group_by_check
    CHECK (group_by IN ('repo', 'lang', 'path', 'author'));
