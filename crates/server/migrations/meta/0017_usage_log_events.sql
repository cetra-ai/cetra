-- GenAI content log records received on /v1/logs and dropped (D-G-40).
ALTER TABLE usage_daily ADD COLUMN genai_log_events INTEGER NOT NULL DEFAULT 0;
