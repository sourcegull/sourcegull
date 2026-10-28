-- Usage analytics (§6.23): rollups of the local `event_logs`, written by the worker's
-- leader-elected rollup job (`seeker-analytics`). Raw events are pruned after
-- `eventLogs.retention`; the rollups are kept for `eventLogs.rollupRetention`. Nothing leaves
-- the instance. No foreign keys on user ids: rollups outlive the user.

-- Events per UTC day, name, source and user (or anonymous visitor). Distinct users over any
-- range (weekly, monthly) are exact from these rows.
CREATE TABLE event_logs_daily (
    day               date    NOT NULL,
    name              text    NOT NULL,
    source            text    NOT NULL,
    user_id           bigint,
    -- '' for signed-in users and for anonymous events without a visitor id.
    anonymous_user_id text    NOT NULL DEFAULT '',
    count             integer NOT NULL CHECK (count > 0)
);
CREATE INDEX event_logs_daily_day_name_idx ON event_logs_daily (day, name);
CREATE INDEX event_logs_daily_user_idx ON event_logs_daily (user_id, day) WHERE user_id IS NOT NULL;

-- Active users (DAU/WAU/MAU) per UTC day, ISO week (from Monday) and month.
CREATE TABLE event_logs_active_users (
    period           text    NOT NULL CHECK (period IN ('day', 'week', 'month')),
    period_start     date    NOT NULL,
    users            integer NOT NULL,
    registered_users integer NOT NULL,
    anonymous_users  integer NOT NULL,
    PRIMARY KEY (period, period_start)
);

-- Searches per day, type (keyword, regexp, structural, symbol, diff, …) and latency bucket
-- (upper bound in ms; 2147483647 for slower, -1 when unknown).
CREATE TABLE event_logs_search_daily (
    day               date    NOT NULL,
    search_type       text    NOT NULL,
    latency_bucket_ms integer NOT NULL,
    count             integer NOT NULL,
    PRIMARY KEY (day, search_type, latency_bucket_ms)
);

-- Code navigation events per day, event name, repository and language.
CREATE TABLE event_logs_codeintel_daily (
    day       date    NOT NULL,
    name      text    NOT NULL,
    repo_name text    NOT NULL,
    language  text    NOT NULL,
    count     integer NOT NULL,
    PRIMARY KEY (day, name, repo_name, language)
);

-- Days rolled up. A day rolled up after it ended is complete and not rolled up again.
CREATE TABLE event_logs_rollup_days (
    day          date        PRIMARY KEY,
    rolled_up_at timestamptz NOT NULL,
    complete     boolean     NOT NULL
);

-- Last activity per user for the admin user list.
DROP INDEX IF EXISTS event_logs_user_id_idx;
CREATE INDEX event_logs_user_id_timestamp_idx ON event_logs (user_id, timestamp) WHERE user_id IS NOT NULL;
