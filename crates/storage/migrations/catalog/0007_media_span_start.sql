-- The newest start_time (ns) of a span that referenced the blob (D-P-21).
ALTER TABLE media ADD COLUMN newest_span_start INTEGER NOT NULL DEFAULT 0;
