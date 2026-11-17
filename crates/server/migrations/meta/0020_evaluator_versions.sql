-- One row per version of an evaluator (D-I-49). Never add a *_enc column here: the history
-- must not hold a sealed value (crypto::encrypted_columns_cover_the_schema would need it).
CREATE TABLE evaluator_versions (
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  config TEXT NOT NULL,               -- as evaluators.config: no sealed value
  output_score TEXT NOT NULL,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (evaluator_id, version)
);
