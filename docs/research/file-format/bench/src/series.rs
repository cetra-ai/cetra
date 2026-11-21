//! The tests of balicat/columnar-format-benchmarks (`format_benchmark.py` and
//! `schema_opt_bench.py`) on Strata, Parquet and Vortex, through DataFusion.
//!
//! The data is a long table of time series, `series_id | period | value`, read from the
//! layout `schema_opt_bench.py` reads: `<data>/<DS>/year=YYYY/data.parquet` with the columns
//! `series_id`, `period`, `period_norm` (strings) and `value` (f64). Each dataset becomes four
//! schema variants, as in `schema_opt_bench.py`:
//!
//! - `v0_today`: series_id dictionary, period string, period_norm string, value;
//! - `v1_3col`: series_id dictionary, period string, value;
//! - `v2_date32`: series_id dictionary, period date32, value;
//! - `v3_d32sort`: `v2_date32` sorted by (series_id, period).
//!
//! Each variant is written as one file per format. Then, warm, `runs` times each: a full read
//! into Arrow (`SELECT *`), and the three queries of `format_benchmark.py`: a count, every row
//! of one series (20 series, timed per series), and an average per period.

use std::collections::HashMap;
use std::fs::File;

use arrow::array::{DictionaryArray, Float64Array, Int32Array, StringArray, UInt32Array};
use arrow::datatypes::Int32Type;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use super::*;

const VARIANTS: [&str; 4] = ["v0_today", "v1_3col", "v2_date32", "v3_d32sort"];
const FORMATS: [&str; 4] = ["pq", "vx_default", "vx_compact", "st"];
const BATCH: usize = 65_536;
const POINT_IDS: usize = 20;

/// `fmtbench series <data> <out> [runs] [datasets...]`.
pub(crate) async fn series_cmd(data: &Path, out: &Path, runs: usize, datasets: &[String]) -> R<()> {
    let datasets: Vec<String> = if datasets.is_empty() { vec!["PET".into(), "ELEC".into()] } else { datasets.to_vec() };
    println!(
        "{:5} {:11} {:10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "ds", "variant", "format", "MB", "write ms", "read med", "read min", "read cpu", "count", "agg med", "agg min", "point med", "point min", "point KiB"
    );
    for ds in &datasets {
        let t = Instant::now();
        let base = load(data, ds)?;
        eprintln!("{ds}: {} rows, {} MiB of Arrow, loaded in {:.1?}", base.num_rows(), base.get_array_memory_size() >> 20, t.elapsed());
        let sids = point_ids(&base)?;
        for variant in VARIANTS {
            if std::env::var("FMT_VARIANTS").is_ok_and(|f| !f.split(',').any(|x| x == variant)) {
                continue;
            }
            let table = make_variant(&base, variant)?;
            let dir = out.join(ds).join(variant);
            for format in FORMATS {
                if std::env::var("FMT_FORMATS").is_ok_and(|f| !f.split(',').any(|x| x == format)) {
                    continue;
                }
                let sub = dir.join(format);
                let _ = std::fs::remove_dir_all(&sub);
                std::fs::create_dir_all(&sub)?;
                let t = Instant::now();
                let res = write(&table, &sub, format, variant == "v3_d32sort").await;
                let write_ms = t.elapsed().as_secs_f64() * 1e3;
                if let Err(e) = res {
                    println!("{ds:5} {variant:11} {format:10} WRITE ERROR {}", first_line(&e));
                    continue;
                }
                let bytes: u64 = std::fs::read_dir(&sub)?.filter_map(Result::ok).map(|e| e.metadata().map_or(0, |m| m.len())).sum();
                match measure_reads(&dir, format, &sids, runs).await {
                    Ok(m) => println!(
                        "{ds:5} {variant:11} {format:10} {:>9.1} {:>9.0} {:>9.1} {:>9.1} {:>9.1} {:>9.2} {:>9.1} {:>9.1} {:>9.3} {:>9.3} {:>9.1}",
                        bytes as f64 / 1e6, write_ms, m.read.0, m.read.1, m.read.2, m.count.1, m.agg.0, m.agg.1, m.point.0, m.point.1, m.point_kib
                    ),
                    Err(e) => println!("{ds:5} {variant:11} {format:10} {:>9.1} {:>9.0} READ ERROR {}", bytes as f64 / 1e6, write_ms, first_line(&e)),
                }
            }
            if std::env::var_os("FMT_KEEP").is_none() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
    Ok(())
}

/// The dataset as one batch: series_id as a dictionary (one per dataset), period and
/// period_norm as strings, value as f64.
fn load(data: &Path, ds: &str) -> R<RecordBatch> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(data.join(ds))?
        .filter_map(Result::ok)
        .map(|e| e.path().join("data.parquet"))
        .filter(|p| p.exists())
        .collect();
    paths.sort();
    let mut ids: HashMap<String, i32> = HashMap::new();
    let mut values: Vec<String> = Vec::new();
    let (mut keys, mut period, mut norm, mut value) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in &paths {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(p)?)?.with_batch_size(BATCH).build()?;
        for b in reader {
            let b = b?;
            let col = |n: &str| -> R<ArrayRef> { Ok(cast(b.column_by_name(n).ok_or("missing column")?, &DataType::Utf8)?) };
            let (sid, per, pn) = (col("series_id")?, col("period")?, col("period_norm")?);
            let (sid, per, pn) = (sid.as_any().downcast_ref::<StringArray>().ok_or("series_id")?, per.as_any().downcast_ref::<StringArray>().ok_or("period")?, pn.as_any().downcast_ref::<StringArray>().ok_or("period_norm")?);
            let v = b.column_by_name("value").ok_or("missing value")?.as_any().downcast_ref::<Float64Array>().ok_or("value")?;
            for i in 0..b.num_rows() {
                let s = sid.value(i);
                let k = match ids.get(s) {
                    Some(k) => *k,
                    None => {
                        let k = i32::try_from(values.len())?;
                        ids.insert(s.to_owned(), k);
                        values.push(s.to_owned());
                        k
                    }
                };
                keys.push(k);
                period.push(per.value(i).to_owned());
                norm.push(pn.value(i).to_owned());
                value.push(v.value(i));
            }
        }
    }
    let sid = DictionaryArray::<Int32Type>::try_new(Int32Array::from(keys), Arc::new(StringArray::from(values)))?;
    let schema = Schema::new(vec![
        Field::new("series_id", sid.data_type().clone(), false),
        Field::new("period", DataType::Utf8, false),
        Field::new("period_norm", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
    ]);
    Ok(RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(sid), Arc::new(StringArray::from(period)), Arc::new(StringArray::from(norm)), Arc::new(Float64Array::from(value))],
    )?)
}

/// `POINT_IDS` series spread over the dictionary, which is in first-seen order.
fn point_ids(base: &RecordBatch) -> R<Vec<String>> {
    let sid = base.column(0).as_any().downcast_ref::<DictionaryArray<Int32Type>>().ok_or("series_id")?;
    let values = sid.values().as_any().downcast_ref::<StringArray>().ok_or("series_id values")?;
    Ok((0..POINT_IDS).map(|k| values.value(k * values.len() / POINT_IDS).to_owned()).collect())
}

/// One schema variant of the base table, as `schema_opt_bench.py` builds it.
fn make_variant(base: &RecordBatch, variant: &str) -> R<RecordBatch> {
    let col = |n: &str| -> R<ArrayRef> { Ok(base.column_by_name(n).ok_or("missing column")?.clone()) };
    let field = |n: &str| -> R<Field> { Ok(base.schema().field_with_name(n)?.clone()) };
    if variant == "v0_today" {
        return Ok(base.clone());
    }
    if variant == "v1_3col" {
        let schema = Schema::new(vec![field("series_id")?, field("period")?, field("value")?]);
        return Ok(RecordBatch::try_new(Arc::new(schema), vec![col("series_id")?, col("period")?, col("value")?])?);
    }
    // Quarterly periods are normalized as "2001-Q1-01": map them to the quarter's first month.
    let norm = col("period_norm")?;
    let norm = norm.as_any().downcast_ref::<StringArray>().ok_or("period_norm")?;
    let fixed: StringArray = norm
        .iter()
        .map(|s| {
            s.map(|s| match s.get(4..8) {
                Some("-Q1-") => format!("{}-01-{}", &s[..4], &s[8..]),
                Some("-Q2-") => format!("{}-04-{}", &s[..4], &s[8..]),
                Some("-Q3-") => format!("{}-07-{}", &s[..4], &s[8..]),
                Some("-Q4-") => format!("{}-10-{}", &s[..4], &s[8..]),
                _ => s.to_owned(),
            })
        })
        .collect();
    let date: ArrayRef = cast(&fixed, &DataType::Date32)?;
    if date.null_count() > 0 {
        return Err(format!("{} periods are not dates", date.null_count()).into());
    }
    let schema = Arc::new(Schema::new(vec![field("series_id")?, Field::new("period", DataType::Date32, false), field("value")?]));
    let v2 = RecordBatch::try_new(schema, vec![col("series_id")?, date, col("value")?])?;
    if variant == "v2_date32" {
        return Ok(v2);
    }
    // Sort by (series_id, period): series ids by their string order, through the rank of each
    // dictionary value.
    let sid = v2.column(0).as_any().downcast_ref::<DictionaryArray<Int32Type>>().ok_or("series_id")?;
    let values = sid.values().as_any().downcast_ref::<StringArray>().ok_or("series_id values")?;
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values.value(*a).cmp(values.value(*b)));
    let mut rank = vec![0u32; values.len()];
    for (r, v) in order.into_iter().enumerate() {
        rank[v] = u32::try_from(r)?;
    }
    let ranks: UInt32Array = sid.keys().values().iter().map(|k| rank[*k as usize]).collect();
    let idx = lexsort_to_indices(
        &[SortColumn { values: Arc::new(ranks), options: None }, SortColumn { values: v2.column(1).clone(), options: None }],
        None,
    )?;
    Ok(take_record_batch(&v2, &idx)?)
}

/// The batch with its dictionary columns as plain strings (for Strata, which builds its own
/// dictionaries).
fn plain(b: &RecordBatch) -> R<RecordBatch> {
    let fields: Vec<Field> = b
        .schema()
        .fields()
        .iter()
        .map(|f| match f.data_type() {
            DataType::Dictionary(_, v) => f.as_ref().clone().with_data_type(v.as_ref().clone()),
            _ => f.as_ref().clone(),
        })
        .collect();
    let cols: Vec<ArrayRef> = b.columns().iter().zip(&fields).map(|(c, f)| cast(c, f.data_type())).collect::<Result<_, _>>()?;
    Ok(RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)?)
}

fn slices(b: &RecordBatch) -> impl Iterator<Item = RecordBatch> + '_ {
    (0..b.num_rows()).step_by(BATCH).map(|off| b.slice(off, BATCH.min(b.num_rows() - off)))
}

/// Writes `table` as one file of `format` into `dir`.
async fn write(table: &RecordBatch, dir: &Path, format: &str, sorted: bool) -> R<()> {
    match format {
        "pq" => {
            // pyarrow's `write_table(compression="zstd")`: zstd at the codec's default level 1,
            // dictionary encoding on, row groups of up to 1 Mi rows.
            let props = WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default())).build();
            let mut w = ArrowWriter::try_new(File::create(dir.join("data.parquet"))?, table.schema(), Some(props))?;
            for s in slices(table) {
                w.write(&s)?;
            }
            w.close()?;
        }
        "vx_default" | "vx_compact" => {
            let session = VortexSession::default().with_tokio();
            let chunks: Vec<_> = slices(table).map(|s| vortex_arrow::convert::from_arrow_batch(&s, false)).collect::<Result<_, _>>()?;
            let arr = ChunkedArray::from_iter(chunks).into_array();
            let mut f = tokio::fs::File::create(dir.join("data.vortex")).await?;
            let mut opts = session.write_options();
            if format == "vx_compact" {
                let strategy = vortex::file::WriteStrategyBuilder::from_session(&session)
                    .with_btrblocks_builder(vortex::compressor::BtrBlocksCompressorBuilder::from_session(&session).with_compact())
                    .build();
                opts = opts.with_strategy(strategy);
            }
            opts.write(&mut f, arr.to_array_stream()).await?;
        }
        "st" => {
            // No sort key: the rows stay in the order given, as in the other formats. Sorted
            // data gets a key filter on series_id.
            let mut cfg = Config::default();
            if sorted {
                cfg.key_filter_columns = vec!["series_id".into()];
            }
            let mut w: Option<StrataWriter> = None;
            for s in slices(table) {
                let s = plain(&s)?;
                let w = match &mut w {
                    Some(w) => w,
                    None => w.insert(StrataWriter::new(s.schema(), &[], cfg.clone())?),
                };
                w.write(&s)?;
            }
            let (bytes, _) = w.ok_or("empty table")?.finish()?;
            std::fs::write(dir.join("data.strata"), bytes)?;
        }
        other => return Err(format!("unknown format {other}").into()),
    }
    Ok(())
}

struct Reads {
    /// (median ms, min ms, median CPU ms)
    read: (f64, f64, f64),
    count: (f64, f64),
    agg: (f64, f64),
    /// Per series.
    point: (f64, f64),
    point_kib: f64,
}

async fn measure_reads(dir: &Path, format: &str, sids: &[String], runs: usize) -> R<Reads> {
    let loc = location(dir.to_str().ok_or("path")?)?;
    let ctx = new_ctx(&loc, None)?;
    table(&ctx, &loc, format).await?;
    let read = timed(&ctx, &["SELECT * FROM t".to_owned()], runs, 1.0).await?;
    let count = timed(&ctx, &["SELECT count(*) FROM t".to_owned()], runs, 1.0).await?;
    let agg = timed(&ctx, &["SELECT period, avg(value) AS a FROM t GROUP BY period".to_owned()], runs, 1.0).await?;
    let points: Vec<String> = sids.iter().map(|s| format!("SELECT period, value FROM t WHERE series_id = '{}'", s.replace('\'', "''"))).collect();
    // Bytes read by the point queries: one pass, warm (files open, caches filled).
    let _ = timed(&ctx, &points, 1, 1.0).await?;
    loc.store.reset();
    let point = timed(&ctx, &points, runs, points.len() as f64).await?;
    let point_kib = loc.store.stats().1 as f64 / 1024.0 / (runs * points.len()) as f64;
    Ok(Reads { read: (read.0, read.1, read.2), count: (count.0, count.1), agg: (agg.0, agg.1), point: (point.0, point.1), point_kib })
}

/// Runs the queries `runs` times after one warm-up; returns the median and minimum time and the
/// median CPU time of one pass, divided by `per`.
async fn timed(ctx: &SessionContext, sqls: &[String], runs: usize, per: f64) -> R<(f64, f64, f64)> {
    let mut rows = 0usize;
    for s in sqls {
        rows += ctx.sql(s).await?.collect().await?.iter().map(RecordBatch::num_rows).sum::<usize>();
    }
    if rows == 0 {
        return Err("no rows".into());
    }
    let (mut times, mut cpus) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for _ in 0..runs {
        let (t, c) = (Instant::now(), ProcessTime::now());
        for s in sqls {
            let batches = ctx.sql(s).await?.collect().await?;
            std::hint::black_box(&batches);
        }
        times.push(t.elapsed().as_secs_f64() * 1e3 / per);
        cpus.push(c.elapsed().as_secs_f64() * 1e3 / per);
    }
    times.sort_by(f64::total_cmp);
    cpus.sort_by(f64::total_cmp);
    Ok((times[times.len() / 2], times[0], cpus[cpus.len() / 2]))
}

#[cfg(test)]
mod tests {
    use arrow::array::Date32Array;

    use super::*;

    fn base() -> RecordBatch {
        let keys = Int32Array::from(vec![1, 0, 1, 0, 2]);
        let sid = DictionaryArray::<Int32Type>::try_new(keys, Arc::new(StringArray::from(vec!["b", "a", "c"]))).unwrap();
        let schema = Schema::new(vec![
            Field::new("series_id", sid.data_type().clone(), false),
            Field::new("period", DataType::Utf8, false),
            Field::new("period_norm", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(sid),
                Arc::new(StringArray::from(vec!["2001Q2", "2003", "2001Q1", "200201", "20020305"])),
                Arc::new(StringArray::from(vec!["2001-Q2-01", "2003-01-01", "2001-Q1-01", "2002-01-01", "2002-03-05"])),
                Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 4.0, 5.0])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn the_sorted_variant_orders_by_series_then_date() {
        let v3 = make_variant(&base(), "v3_d32sort").unwrap();
        let plain = plain(&v3).unwrap();
        let sid = plain.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        let date = plain.column(1).as_any().downcast_ref::<Date32Array>().unwrap();
        let value = plain.column(2).as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(sid.iter().flatten().collect::<Vec<_>>(), ["a", "a", "b", "b", "c"]);
        // a: 2001Q1 (3.0), 2001Q2 (1.0); b: 200201 (4.0), 2003 (2.0); c.
        assert_eq!(value.values().to_vec(), [3.0, 1.0, 4.0, 2.0, 5.0]);
        // 2001-04-01 is day 11,413 since the epoch.
        assert_eq!(date.value(1), 11_413);
        assert_eq!(make_variant(&base(), "v1_3col").unwrap().num_columns(), 3);
    }
}
