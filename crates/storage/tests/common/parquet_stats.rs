//! Compressed and uncompressed sizes of the spans Statam files, per column
//! (track R M7, R7.3). Local object store only.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use cetra_statam::{FileReader, StatamFileMeta};
use sqlx::ConnectOptions;

/// Bytes of a column: `uncompressed` is the encoded block size before outer codec.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Sizes {
    pub(crate) uncompressed: u64,
    pub(crate) compressed: u64,
}

/// Per level ("L0", "L1"), per logical column name: sums over every block of
/// the given `.statam` files (paths relative to `objects_dir`).
pub(crate) fn column_sizes(
    objects_dir: &Path,
    files: &[(u8, String)],
) -> Result<BTreeMap<String, BTreeMap<String, Sizes>>, String> {
    let mut out: BTreeMap<String, BTreeMap<String, Sizes>> = BTreeMap::new();
    for (level, path) in files {
        let reader = FileReader::open(objects_dir.join(path)).map_err(|error| error.to_string())?;
        let meta = StatamFileMeta::read_from(&reader, None).map_err(|error| error.to_string())?;
        let level_sizes = out.entry(format!("L{level}")).or_default();
        let fields = meta.schema_meta.schema.fields();
        for col in &meta.col_dir {
            let name = fields
                .get(usize::from(col.logical_col_id))
                .map(|f| f.name.as_str())
                .unwrap_or("unknown");
            let sizes = level_sizes.entry(name.to_owned()).or_default();
            let start = col.first_block_idx as usize;
            let end = start.saturating_add(col.num_blocks as usize);
            if let Some(blocks) = meta.block_dir.get(start..end) {
                for blk in blocks {
                    sizes.uncompressed += u64::from(blk.raw_len);
                    sizes.compressed += u64::from(blk.stored_len);
                }
            }
        }
    }
    Ok(out)
}

/// `(level, path)` of every **active** `spans` file, read from `catalog.db`
/// through a read-only connection.
pub(crate) async fn active_spans_files(data_dir: &Path) -> Result<Vec<(u8, String)>, String> {
    let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(data_dir.join("catalog.db"))
        .read_only(true)
        .busy_timeout(Duration::from_secs(5))
        .connect()
        .await
        .map_err(|error| error.to_string())?;
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT level, path FROM data_files WHERE table_name = 'spans' AND state = 'active'",
    )
    .fetch_all(&mut connection)
    .await
    .map_err(|error| error.to_string())?;
    rows.into_iter()
        .map(|(level, path)| {
            let level = u8::try_from(level).map_err(|_| "level out of range".to_owned())?;
            Ok((level, path))
        })
        .collect()
}
