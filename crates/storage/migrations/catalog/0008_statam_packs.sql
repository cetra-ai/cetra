ALTER TABLE data_files ADD COLUMN pack_id BLOB;
ALTER TABLE data_files ADD COLUMN pack_path TEXT;
ALTER TABLE data_files ADD COLUMN pack_size_bytes INTEGER;
ALTER TABLE data_files ADD COLUMN pack_toc_size INTEGER;

CREATE INDEX data_files_pack_path
    ON data_files (pack_path)
    WHERE pack_path IS NOT NULL;
