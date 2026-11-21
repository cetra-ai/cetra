//! Tuning parameters for the writer and the reader.

/// Tuning parameters. [`Config::default`] gives the values documented on each field.
#[derive(Debug, Clone)]
pub struct Config {
    /// zstd level for narrow, refs, header and table segments. Default 3.
    pub zstd_level: i32,
    /// zstd level for payload frames, shared store and own stores alike. Each store of at least
    /// 4 MiB of unique chunks also trains a dictionary. Default 3.
    pub payload_zstd_level: i32,
    /// Maximum rows per narrow chunk. Default 2,048: a trace lookup reads one chunk per column.
    /// The writer rejects values above 65,535 (the `FSB_RLE` run lengths are `u16`).
    pub chunk_max_rows: u32,
    /// Maximum value bytes per narrow chunk. Default 32 KiB.
    pub chunk_max_bytes: usize,
    /// Maximum block-table entries per page. Default 64. The writer rejects 0 and values
    /// above 4,096 (`MAX_PAGE_ENTRIES`).
    pub page_max_entries: usize,
    /// Maximum distinct values in a dictionary. Default 65,535. The writer rejects values
    /// above 1,048,576 (`MAX_DICT_VALUES`).
    pub dict_max_values: usize,
    /// Maximum dictionary size in bytes. Default 1 MiB.
    pub dict_max_bytes: usize,
    /// A Utf8 or Binary column whose average non-null value is at least this many bytes is
    /// stored with the payload layer. Default 256.
    pub payload_min_avg: usize,
    /// Payload columns that get a chunk store of their own instead of the file's shared one:
    /// short texts such as previews, which would fill the shared store with near copies of the
    /// payloads. Default `["input_preview", "output_preview"]`.
    pub own_store_columns: Vec<String>,
    /// Columns, besides the first sort-key column, that get a key filter. Names absent from the
    /// schema, or of an unsupported type, are ignored. Default `["user_id", "session_id"]`.
    pub key_filter_columns: Vec<String>,
    /// Suffix read length when no tail length hint is given. Default 1 MiB, which holds the
    /// metadata section and the hot tail of most files; a longer tail costs one more read.
    pub open_default_suffix: u64,
    /// Largest metadata section an open reads with the hot tail. Default 4 MiB. A larger one is
    /// read column by column, as queries need it, which costs a wave of requests the first time.
    pub open_metadata_max: u64,
    /// Threads the writer compresses and indexes payload frames on. Default: the number of
    /// cores. The file is the same for any value; 1 writes on the calling thread only.
    pub write_threads: usize,
}

impl Config {
    /// The configuration for rewriting cold files, those no longer ingested into: every zstd
    /// level at 19. It writes at about 10 MB/s, so it belongs in background compaction, never in
    /// ingestion. It reads back about 10% slower, and the files are 10 to 30% smaller.
    pub fn cold() -> Self {
        Self {
            zstd_level: 19,
            payload_zstd_level: 19,
            ..Self::default()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            zstd_level: 3,
            payload_zstd_level: 3,
            write_threads: crate::parallel::cores(),
            chunk_max_rows: 2_048,
            chunk_max_bytes: 32 * 1024,
            page_max_entries: 64,
            dict_max_values: 65_535,
            dict_max_bytes: 1024 * 1024,
            payload_min_avg: 256,
            own_store_columns: vec!["input_preview".into(), "output_preview".into()],
            key_filter_columns: vec!["user_id".into(), "session_id".into()],
            open_default_suffix: 1024 * 1024,
            open_metadata_max: 4 * 1024 * 1024,
        }
    }
}
