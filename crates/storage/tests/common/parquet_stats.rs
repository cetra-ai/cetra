//! Compressed and uncompressed sizes of the spans Parquet files, per column
//! (track R M7, R7.3). Local object store only.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use sqlx::ConnectOptions;

/// Bytes of a column chunk: `uncompressed` is the encoded page size before zstd.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Sizes {
    pub(crate) uncompressed: u64,
    pub(crate) compressed: u64,
}

/// Per level ("L0", "L1"), per leaf column path: sums over every row group of
/// the given files (paths relative to `objects_dir`).
pub(crate) fn column_sizes(
    objects_dir: &Path,
    files: &[(u8, String)],
) -> Result<BTreeMap<String, BTreeMap<String, Sizes>>, String> {
    let mut out: BTreeMap<String, BTreeMap<String, Sizes>> = BTreeMap::new();
    for (level, path) in files {
        let file = File::open(objects_dir.join(path)).map_err(|error| error.to_string())?;
        let reader = SerializedFileReader::new(file).map_err(|error| error.to_string())?;
        let level_sizes = out.entry(format!("L{level}")).or_default();
        for row_group in reader.metadata().row_groups() {
            for column in row_group.columns() {
                let sizes = level_sizes
                    .entry(column.column_path().string())
                    .or_default();
                sizes.uncompressed += column.uncompressed_size().max(0) as u64;
                sizes.compressed += column.compressed_size().max(0) as u64;
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
