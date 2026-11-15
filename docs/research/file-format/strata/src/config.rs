//! Tuning parameters for the writer and the reader.

/// Tuning parameters. [`Config::default`] gives the values documented on each field.
#[derive(Debug, Clone)]
pub struct Config {
    /// zstd level for narrow, frame and table segments. Default 3.
    pub zstd_level: i32,
    /// Maximum rows per narrow chunk. Default 4,096. The writer rejects values above 65,535
    /// (the `FSB_RLE` run lengths are `u16`).
    pub chunk_max_rows: u32,
    /// Maximum value bytes per narrow chunk. Default 32 KiB.
    pub chunk_max_bytes: usize,
    /// Maximum block-table entries per page. Default 64. The writer rejects 0 and values
    /// above 4,096 (`MAX_PAGE_ENTRIES`).
    pub page_max_entries: usize,
    /// Maximum distinct values in a dictionary. Default 65,535. The writer rejects values
    /// above 1,048,576 (`MAX_DICT_VALUES`).
    pub dict_max_values: usize,
    /// Maximum dictionary size in bytes. Default 64 KiB.
    pub dict_max_bytes: usize,
    /// Suffix read length when no hot-tail length hint is given. Default 256 KiB.
    pub open_default_suffix: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            zstd_level: 3,
            chunk_max_rows: 4_096,
            chunk_max_bytes: 32 * 1024,
            page_max_entries: 64,
            dict_max_values: 65_535,
            dict_max_bytes: 64 * 1024,
            open_default_suffix: 256 * 1024,
        }
    }
}
