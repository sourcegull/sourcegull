-- Code Insights views and dashboards (docs/PLAN.md §6.15), the API half on top of the
-- engine's series (`20261114000001_insights`). The concepts follow the original's
-- `internal/insights` (read for behaviour only): a view is a chart (line or pie) over one or
-- more series, series are shared by every view that defines the same series, and views and
-- dashboards are visible through grants to users, organizations or everyone.

-- An insight view: a line chart over series, or a pie chart of language statistics.
CREATE TABLE insight_views (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Stable public identifier (the original's `unique_id`), in the view's node ID.
    unique_id text NOT NULL UNIQUE,
    title text NOT NULL DEFAULT '',
    presentation_type text NOT NULL CHECK (presentation_type IN ('LINE', 'PIE')),
    -- Default filters, applied when points are read.
    filter_include_repo_regex text,
    filter_exclude_repo_regex text,
    filter_search_contexts text[] NOT NULL DEFAULT '{}',
    -- Default series display options.
    series_sort_mode text CHECK (series_sort_mode IN ('RESULT_COUNT', 'LEXICOGRAPHICAL', 'DATE_ADDED')),
    series_sort_direction text CHECK (series_sort_direction IN ('ASC', 'DESC')),
    series_limit integer CHECK (series_limit >= 0),
    series_num_samples integer CHECK (series_num_samples > 0),
    -- Pie charts: languages under this share of the lines fall into "Other", and the query
    -- and repositories whose language statistics the chart shows.
    other_threshold double precision,
    pie_query text,
    pie_repositories text[] NOT NULL DEFAULT '{}',
    created_by bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Who sees a view directly (also visible through any dashboard it is on). Creating a view
-- grants it to its creator.
CREATE TABLE insight_view_grants (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    view_id bigint NOT NULL REFERENCES insight_views (id) ON DELETE CASCADE,
    user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    global boolean NOT NULL DEFAULT false,
    CHECK (num_nonnulls(user_id, org_id) + global::int = 1)
);
CREATE INDEX insight_view_grants_view_idx ON insight_view_grants (view_id);
CREATE INDEX insight_view_grants_user_idx ON insight_view_grants (user_id) WHERE user_id IS NOT NULL;
CREATE INDEX insight_view_grants_org_idx ON insight_view_grants (org_id) WHERE org_id IS NOT NULL;

-- The series of a view with their presentation; a series may be on many views.
CREATE TABLE insight_view_series (
    view_id bigint NOT NULL REFERENCES insight_views (id) ON DELETE CASCADE,
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    label text NOT NULL DEFAULT '',
    stroke text NOT NULL DEFAULT '',
    -- Order of the series on the view.
    position integer NOT NULL DEFAULT 0,
    PRIMARY KEY (view_id, series_id)
);
CREATE INDEX insight_view_series_series_idx ON insight_view_series (series_id);

-- A dashboard of views. Deleting one is soft (as the original), so it can be restored.
CREATE TABLE insight_dashboards (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title text NOT NULL,
    created_by bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);

-- Who sees (and edits) a dashboard: users, organizations' members, or everyone.
CREATE TABLE insight_dashboard_grants (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    dashboard_id bigint NOT NULL REFERENCES insight_dashboards (id) ON DELETE CASCADE,
    user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    global boolean NOT NULL DEFAULT false,
    CHECK (num_nonnulls(user_id, org_id) + global::int = 1)
);
CREATE INDEX insight_dashboard_grants_dashboard_idx ON insight_dashboard_grants (dashboard_id);
CREATE INDEX insight_dashboard_grants_user_idx ON insight_dashboard_grants (user_id)
    WHERE user_id IS NOT NULL;
CREATE INDEX insight_dashboard_grants_org_idx ON insight_dashboard_grants (org_id)
    WHERE org_id IS NOT NULL;

-- The views on a dashboard, in the order they were added.
CREATE TABLE insight_dashboard_views (
    id bigint GENERATED ALWAYS AS IDENTITY,
    dashboard_id bigint NOT NULL REFERENCES insight_dashboards (id) ON DELETE CASCADE,
    view_id bigint NOT NULL REFERENCES insight_views (id) ON DELETE CASCADE,
    PRIMARY KEY (dashboard_id, view_id)
);
CREATE INDEX insight_dashboard_views_view_idx ON insight_dashboard_views (view_id);
