-- The last test of a connection's saved values (D-I-54).
ALTER TABLE llm_connections ADD COLUMN last_tested_at INTEGER;
ALTER TABLE llm_connections ADD COLUMN last_test_ok INTEGER;
