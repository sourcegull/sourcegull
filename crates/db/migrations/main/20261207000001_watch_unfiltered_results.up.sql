-- A watch whose email recipients get every result unfiltered: who turned it on and when.
-- NULL: each recipient gets only the results in repositories they can read.
ALTER TABLE cm_monitors
    ADD COLUMN unfiltered_by bigint REFERENCES users (id) ON DELETE SET NULL,
    ADD COLUMN unfiltered_at timestamptz;
