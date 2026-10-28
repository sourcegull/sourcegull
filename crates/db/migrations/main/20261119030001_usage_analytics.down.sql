DROP INDEX IF EXISTS event_logs_user_id_timestamp_idx;
CREATE INDEX IF NOT EXISTS event_logs_user_id_idx ON event_logs (user_id) WHERE user_id IS NOT NULL;
DROP TABLE IF EXISTS event_logs_rollup_days;
DROP TABLE IF EXISTS event_logs_codeintel_daily;
DROP TABLE IF EXISTS event_logs_search_daily;
DROP TABLE IF EXISTS event_logs_active_users;
DROP TABLE IF EXISTS event_logs_daily;
