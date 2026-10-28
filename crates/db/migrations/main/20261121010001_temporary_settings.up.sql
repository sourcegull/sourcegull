-- Temporary settings (docs/PLAN.md §6.3, §5.1): small per-user UI state that is not part of
-- the settings cascade and has no history, such as recent searches
-- (`search.input.recentSearches`), as in the original's `temporary_settings` table. One JSON
-- object per user, read and written through GraphQL `temporarySettings`,
-- `overwriteTemporarySettings` and `editTemporarySettings`.
CREATE TABLE temporary_settings (
    user_id bigint PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    contents jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT temporary_settings_contents_object CHECK (jsonb_typeof(contents) = 'object')
);
