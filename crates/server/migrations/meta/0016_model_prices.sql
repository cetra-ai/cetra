CREATE TABLE model_prices (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
  model_name TEXT NOT NULL,
  match_pattern TEXT NOT NULL,
  provider TEXT,
  valid_from INTEGER NOT NULL DEFAULT 0,
  price_input REAL,
  price_output REAL,
  price_cache_read REAL,
  price_cache_write REAL,
  price_reasoning REAL,
  tiers TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX model_prices_key ON model_prices(
  org_id, coalesce(project_id, ''), match_pattern, coalesce(provider, ''), valid_from
);
CREATE INDEX model_prices_project ON model_prices(project_id) WHERE project_id IS NOT NULL;
