-- Request counters of the keyed rate limiter (`seeker_ratelimit::PgKeyedLimiter`): one fixed
-- window per key (`<limit name>:<client address or account>`). Rows whose window ended are
-- deleted by the limiter from time to time.
CREATE UNLOGGED TABLE rate_limit_counters (
    key          text        PRIMARY KEY,
    window_start timestamptz NOT NULL,
    hits         integer     NOT NULL
);

CREATE INDEX rate_limit_counters_window_start ON rate_limit_counters (window_start);

-- Failed password sign-ins per account and client address (`auth.lockout`). A lock applies
-- only to the address whose failures caused it; `address` is '' when it is not known.
CREATE TABLE sign_in_failures (
    user_id      bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    address      text        NOT NULL,
    -- Consecutive failures of the current run.
    failures     integer     NOT NULL DEFAULT 0,
    -- First failure of the current run.
    since        timestamptz,
    locked_until timestamptz,
    PRIMARY KEY (user_id, address)
);
