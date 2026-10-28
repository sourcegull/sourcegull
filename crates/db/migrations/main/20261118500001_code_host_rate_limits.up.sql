-- Code host request budgets shared by every process (docs/PLAN.md §5.4): one token bucket per
-- code host connection (`extsvc:<id>`, or `gitlab:<url>#<token hash>` for clients built
-- without a connection id). Processes reserve tokens in batches with
-- `code_host_rate_limit_take` and hand them out locally; a 429 or an exhausted
-- `RateLimit-Remaining` sets `paused_until`, which stops every process.
CREATE TABLE IF NOT EXISTS code_host_rate_limits (
    key               text             PRIMARY KEY,
    -- `rateLimit.requestsPerHour`; NULL = unlimited (`rateLimit.enabled: false`).
    requests_per_hour double precision CHECK (requests_per_hour >= 0),
    -- Bucket size: one second's worth of requests, at least one.
    burst             double precision NOT NULL,
    -- Tokens left at `replenished_at`.
    tokens            double precision NOT NULL,
    replenished_at    timestamptz      NOT NULL DEFAULT now(),
    -- The code host asked every client to back off until then.
    paused_until      timestamptz,
    updated_at        timestamptz      NOT NULL DEFAULT now()
);

-- Refills `p_key`'s bucket up to now and takes up to `p_want` whole tokens (none while paused).
-- Creates the bucket (full) on first use and applies a changed rate. Returns the tokens
-- granted, the tokens left and the seconds until the pause ends (0 when not paused).
CREATE OR REPLACE FUNCTION code_host_rate_limit_take(
    p_key text, p_requests_per_hour double precision, p_want double precision)
RETURNS TABLE (granted double precision, tokens_left double precision,
               pause_seconds double precision)
LANGUAGE plpgsql AS $$
DECLARE
    v_burst  double precision := GREATEST(1.0, COALESCE(p_requests_per_hour, 0) / 3600.0);
    v_row    code_host_rate_limits%ROWTYPE;
    v_tokens double precision;
    v_grant  double precision := 0;
    v_pause  double precision := 0;
BEGIN
    INSERT INTO code_host_rate_limits AS r (key, requests_per_hour, burst, tokens)
    VALUES (p_key, p_requests_per_hour, v_burst, v_burst)
    ON CONFLICT (key) DO NOTHING;
    SELECT * INTO v_row FROM code_host_rate_limits r WHERE r.key = p_key FOR UPDATE;

    IF p_requests_per_hour IS NULL THEN
        v_tokens := 0;
    ELSE
        v_tokens := LEAST(v_burst, v_row.tokens + p_requests_per_hour / 3600.0
            * GREATEST(0, extract(epoch FROM now() - v_row.replenished_at)));
    END IF;
    IF v_row.paused_until > now() THEN
        v_pause := extract(epoch FROM v_row.paused_until - now());
    ELSIF p_requests_per_hour IS NULL THEN
        v_grant := p_want;
    ELSE
        v_grant := GREATEST(0, LEAST(p_want, floor(v_tokens)));
        v_tokens := v_tokens - v_grant;
    END IF;

    UPDATE code_host_rate_limits r
    SET requests_per_hour = p_requests_per_hour, burst = v_burst, tokens = v_tokens,
        replenished_at = now(), updated_at = now()
    WHERE r.key = p_key;
    RETURN QUERY SELECT v_grant, v_tokens, v_pause;
END
$$;
