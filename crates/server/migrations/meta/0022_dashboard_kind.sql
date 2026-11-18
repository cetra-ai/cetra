-- The project's Overview is a dashboards row of kind 'overview' (D-C-18). Custom dashboards
-- keep the default kind.
ALTER TABLE dashboards ADD COLUMN kind TEXT NOT NULL DEFAULT 'custom' CHECK (kind IN ('custom', 'overview'));
CREATE UNIQUE INDEX dashboards_overview ON dashboards(project_id) WHERE kind = 'overview';
