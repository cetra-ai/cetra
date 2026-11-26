//! Synchronous Shape B SQL execution drivers (`sql_check`, `sql_json`,
//! `sql_export_stream`, and streaming CSV/NDJSON piece encoders).

use std::sync::Arc;

use cetra_column::{Batch, CancelToken, Reservation, Schema, ScopedPool};
use serde_json::{Map, Value};

use crate::encode::size::{JSON_ARRAY_BYTES, JSON_ROW_SEPARATOR_BYTES, exceeds};
use crate::encode::{
    EXPORT_MAX_ROWS, SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS, SqlJsonResult, batch_row_sizes,
    encode_csv_chunk, encode_csv_header, encode_json_result, encode_ndjson_chunk,
    export_piece_ranges,
};
use crate::error::QueryError;
use crate::exec::pipeline::ExecContext;
use crate::exec::source::PartitionStream;

use super::binder::{BoundParams, bind_statement};
use super::catalog::SqlCatalog;
use super::compile::{SqlSources, compile_plan};
use super::logical::{LogicalPlan, SqlColumn, columns_from_schema};
use super::optimize::optimize;
use super::params::parse_param_values;
use super::validate::parse_and_validate_sql;

/// Parses, validates, binds `{name:Type}` parameters, and optimizes `sql` synchronously.
///
/// All `sqlparser::Parser` (`Rc`) state is dropped before returning, making the returned
/// [`LogicalPlan`] `Send + Sync` for async callers.
pub fn prepare_sql_plan(
    catalog: &SqlCatalog,
    sql: &str,
    params_json: &Map<String, Value>,
) -> Result<LogicalPlan, QueryError> {
    let (stmt, placeholders) = parse_and_validate_sql(sql, &catalog.registered_names())?;
    let param_values = parse_param_values(&placeholders, params_json)?;
    let bound = BoundParams::Values(&param_values);
    let plan = bind_statement(&stmt, catalog, &bound)?;
    optimize(plan)
}

/// Validates, binds, and plans `sql` without executing it, returning output [`SqlColumn`]s.
pub fn sql_check(
    catalog: &SqlCatalog,
    sql: &str,
    params_json: &Map<String, Value>,
) -> Result<Vec<SqlColumn>, QueryError> {
    let plan = prepare_sql_plan(catalog, sql, params_json)?;
    Ok(columns_from_schema(plan.schema()))
}

/// Plans and executes `sql`, pulling at most `limit + 1` rows and encoding up to
/// [`SQL_MAX_RESULT_BYTES`] (32 MiB) into [`SqlJsonResult`].
pub fn sql_json(
    catalog: &SqlCatalog,
    sources: &SqlSources,
    ctx: &ExecContext,
    sql: &str,
    params_json: &Map<String, Value>,
    limit: usize,
) -> Result<SqlJsonResult, QueryError> {
    sql_json_within(
        catalog,
        sources,
        ctx,
        sql,
        params_json,
        limit,
        SQL_MAX_RESULT_BYTES,
    )
}

/// Like [`sql_json`] with an explicit encoded JSON byte cap (primarily for testing).
pub fn sql_json_within(
    catalog: &SqlCatalog,
    sources: &SqlSources,
    ctx: &ExecContext,
    sql: &str,
    params_json: &Map<String, Value>,
    limit: usize,
    max_bytes: usize,
) -> Result<SqlJsonResult, QueryError> {
    validate_json_limit(limit)?;
    let plan = prepare_sql_plan(catalog, sql, params_json)?;
    execute_plan_json(&plan, sources, ctx, limit, max_bytes)
}

/// Executes an already-prepared [`LogicalPlan`] for JSON console output, dropping the
/// execution stream before encoding.
pub fn execute_plan_json(
    plan: &LogicalPlan,
    sources: &SqlSources,
    ctx: &ExecContext,
    limit: usize,
    max_bytes: usize,
) -> Result<SqlJsonResult, QueryError> {
    validate_json_limit(limit)?;
    let schema = Arc::clone(plan.schema());
    let mut stream = compile_plan(plan, sources, ctx)?;

    let mut stream_res = ctx.pool.register("sql_json_stream", false);
    let mut kept_res = ctx.pool.register("sql_json_kept", false);
    let mut kept: Vec<Batch> = Vec::new();
    let mut rows_kept = 0usize;
    let mut est_bytes = JSON_ARRAY_BYTES;
    let mut already_truncated = false;

    loop {
        if ctx.eval.cancel.is_cancelled() {
            drop(stream);
            kept.clear();
            stream_res.free();
            kept_res.free();
            return Err(QueryError::Cancelled);
        }
        let next = stream.next_batch(&ctx.eval.cancel, Some(&mut stream_res));
        let batch = match next {
            Ok(Some(b)) => b,
            Ok(None) => {
                stream_res.free();
                break;
            }
            Err(err) => {
                drop(stream);
                kept.clear();
                stream_res.free();
                kept_res.free();
                return Err(err);
            }
        };
        if batch.is_empty() {
            stream_res.free();
            continue;
        }

        if rows_kept >= limit {
            // We pulled the (limit + 1)-th row: mark truncated and stop pulling.
            already_truncated = true;
            drop(batch);
            stream_res.free();
            break;
        }

        let room = limit - rows_kept;
        let mut batch = if batch.num_rows() > room {
            already_truncated = true;
            batch.slice(0, room)?
        } else {
            batch
        };

        let sizes = batch_row_sizes(&batch)?;
        let mut fits = 0usize;
        for &sz in &sizes {
            let next_est = est_bytes
                .saturating_add(sz)
                .saturating_add(JSON_ROW_SEPARATOR_BYTES);
            if exceeds(next_est, max_bytes) {
                break;
            }
            est_bytes = next_est;
            fits += 1;
        }
        if fits == 0 {
            if rows_kept == 0 {
                drop(batch);
                drop(stream);
                kept.clear();
                stream_res.free();
                kept_res.free();
                return Err(QueryError::TooLarge);
            }
            already_truncated = true;
            drop(batch);
            stream_res.free();
            break;
        }
        if fits < batch.num_rows() {
            already_truncated = true;
            batch = batch.slice(0, fits)?;
        }

        if batch.num_rows().saturating_mul(4) < batch.raw_len() {
            let compacted = batch.compact(Some(&mut kept_res))?;
            drop(batch);
            batch = compacted;
        } else if let Err(err) = kept_res.try_grow(batch.memory_bytes()) {
            drop(batch);
            drop(stream);
            kept.clear();
            stream_res.free();
            kept_res.free();
            return Err(err.into());
        }

        rows_kept += batch.num_rows();
        kept.push(batch);
        stream_res.free();

        if already_truncated {
            break;
        }
    }

    // Drop the execution stream BEFORE encoding so operator state is freed first.
    drop(stream);
    stream_res.free();

    let mut encode_res = ctx.pool.register("sql_json_encode", false);
    let result = encode_json_result(
        &schema,
        &kept,
        limit,
        max_bytes,
        already_truncated,
        Some(&mut encode_res),
    );
    drop(kept);
    kept_res.free();
    encode_res.free();
    result
}

/// Plans `sql` and returns its output schema together with a streaming [`PartitionStream`]
/// capped at [`EXPORT_MAX_ROWS`] (10,000,000) rows.
pub fn sql_export_stream(
    catalog: &SqlCatalog,
    sources: &SqlSources,
    ctx: &ExecContext,
    sql: &str,
    params_json: &Map<String, Value>,
) -> Result<(Arc<Schema>, Box<dyn PartitionStream>), QueryError> {
    let plan = prepare_sql_plan(catalog, sql, params_json)?;
    sql_export_plan_stream(&plan, sources, ctx, EXPORT_MAX_ROWS)
}

/// Compiles an already-prepared [`LogicalPlan`] into an export [`PartitionStream`] capped
/// at `row_cap` data rows.
pub fn sql_export_plan_stream(
    plan: &LogicalPlan,
    sources: &SqlSources,
    ctx: &ExecContext,
    row_cap: usize,
) -> Result<(Arc<Schema>, Box<dyn PartitionStream>), QueryError> {
    let schema = Arc::clone(plan.schema());
    let stream = compile_plan(plan, sources, ctx)?;
    Ok((
        schema,
        Box::new(RowCapStream {
            inner: Some(stream),
            remaining: row_cap,
        }),
    ))
}

/// Encodes one [`Batch`] into one or more RFC 4180 CSV pieces bounded by `piece_bytes`
/// and `max_chunk_bytes`, reserving chunk memory on `pool`.
pub fn encode_csv_batch_pieces(
    batch: &Batch,
    piece_bytes: usize,
    max_chunk_bytes: usize,
    pool: &Arc<ScopedPool>,
    mut emit: impl FnMut(Vec<u8>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    let ranges = export_piece_ranges(batch, piece_bytes, max_chunk_bytes)?;
    for range in ranges {
        let mut res = pool.register("csv_export_chunk", false);
        let bytes = encode_csv_chunk(batch, range, max_chunk_bytes, Some(&mut res))?;
        drop(res);
        if !bytes.is_empty() {
            emit(bytes)?;
        }
    }
    Ok(())
}

/// Encodes one [`Batch`] into one or more NDJSON pieces bounded by `piece_bytes`
/// and `max_chunk_bytes`, reserving chunk memory on `pool`.
pub fn encode_ndjson_batch_pieces(
    batch: &Batch,
    piece_bytes: usize,
    max_chunk_bytes: usize,
    pool: &Arc<ScopedPool>,
    mut emit: impl FnMut(Vec<u8>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    let ranges = export_piece_ranges(batch, piece_bytes, max_chunk_bytes)?;
    for range in ranges {
        let mut res = pool.register("ndjson_export_chunk", false);
        let bytes = encode_ndjson_chunk(batch, range, max_chunk_bytes, Some(&mut res))?;
        drop(res);
        if !bytes.is_empty() {
            emit(bytes)?;
        }
    }
    Ok(())
}

/// Streams a complete CSV export (header + data chunks) from `stream` into `emit`.
pub fn export_csv_stream(
    schema: &Schema,
    stream: &mut dyn PartitionStream,
    ctx: &ExecContext,
    piece_bytes: usize,
    max_chunk_bytes: usize,
    mut emit: impl FnMut(Vec<u8>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    let header = encode_csv_header(schema)?;
    emit(header)?;
    let mut pull_res = ctx.pool.register("csv_export_pull", false);
    loop {
        if ctx.eval.cancel.is_cancelled() {
            pull_res.free();
            return Err(QueryError::Cancelled);
        }
        let next = stream.next_batch(&ctx.eval.cancel, Some(&mut pull_res));
        let Some(batch) = (match next {
            Ok(b) => b,
            Err(err) => {
                pull_res.free();
                return Err(err);
            }
        }) else {
            pull_res.free();
            break;
        };
        if !batch.is_empty() {
            let res =
                encode_csv_batch_pieces(&batch, piece_bytes, max_chunk_bytes, &ctx.pool, &mut emit);
            drop(batch);
            pull_res.free();
            res?;
        } else {
            pull_res.free();
        }
    }
    Ok(())
}

/// Streams a complete NDJSON export from `stream` into `emit`.
pub fn export_ndjson_stream(
    stream: &mut dyn PartitionStream,
    ctx: &ExecContext,
    piece_bytes: usize,
    max_chunk_bytes: usize,
    mut emit: impl FnMut(Vec<u8>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    let mut pull_res = ctx.pool.register("ndjson_export_pull", false);
    loop {
        if ctx.eval.cancel.is_cancelled() {
            pull_res.free();
            return Err(QueryError::Cancelled);
        }
        let next = stream.next_batch(&ctx.eval.cancel, Some(&mut pull_res));
        let Some(batch) = (match next {
            Ok(b) => b,
            Err(err) => {
                pull_res.free();
                return Err(err);
            }
        }) else {
            pull_res.free();
            break;
        };
        if !batch.is_empty() {
            let res = encode_ndjson_batch_pieces(
                &batch,
                piece_bytes,
                max_chunk_bytes,
                &ctx.pool,
                &mut emit,
            );
            drop(batch);
            pull_res.free();
            res?;
        } else {
            pull_res.free();
        }
    }
    Ok(())
}

fn validate_json_limit(limit: usize) -> Result<(), QueryError> {
    if !(1..=SQL_MAX_ROWS).contains(&limit) {
        return Err(QueryError::invalid_request(
            "limit must be between 1 and 10000",
        ));
    }
    Ok(())
}

struct RowCapStream {
    inner: Option<Box<dyn PartitionStream>>,
    remaining: usize,
}

impl PartitionStream for RowCapStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        if cancel.is_cancelled() {
            rollback_res(res, before);
            return Err(QueryError::Cancelled);
        }
        if self.remaining == 0 {
            self.inner = None;
            rollback_res(res, before);
            return Ok(None);
        }
        let Some(inner) = self.inner.as_mut() else {
            rollback_res(res, before);
            return Ok(None);
        };
        let Some(batch) = inner.next_batch(cancel, res.as_deref_mut())? else {
            self.inner = None;
            return Ok(None);
        };
        let rows = batch.num_rows();
        if rows <= self.remaining {
            self.remaining -= rows;
            if self.remaining == 0 {
                self.inner = None;
            }
            return Ok(Some(batch));
        }
        let keep = self.remaining;
        self.remaining = 0;
        self.inner = None;
        let sliced = match batch.slice(0, keep) {
            Ok(b) => b,
            Err(err) => {
                rollback_res(res, before);
                return Err(err.into());
            }
        };
        if let Some(r) = res {
            let target = before.saturating_add(sliced.memory_bytes());
            let cur = r.size();
            if target > cur {
                if let Err(err) = r.try_grow(target - cur) {
                    rollback_res(Some(r), before);
                    return Err(err.into());
                }
            } else if target < cur {
                r.shrink(cur - target);
            }
        }
        Ok(Some(sliced))
    }
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Schema, Utf8Data,
    };

    use super::*;
    use crate::encode::{EXPORT_MAX_CHUNK_BYTES, EXPORT_PIECE_BYTES};
    use crate::exec::{BatchSource, ExecMetrics, SpillManager, WorkerPool};
    use crate::ir::EvalContext;

    fn test_ctx(cap: usize) -> (ExecContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 1_767_225_600_000_000_000,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 64,
            target_partitions: 2,
            metrics: ExecMetrics::default(),
        };
        (ctx, dir)
    }

    fn sample_catalog_and_sources() -> (SqlCatalog, SqlSources) {
        let mut catalog = SqlCatalog::new(None, None);
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("label", DataType::Utf8, true),
        ])
        .unwrap();
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["a", "=1+1", "c"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![4, 5]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["d", "e"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();
        let src = Arc::new(BatchSource::new(schema, vec![b1, b2]).unwrap());
        catalog.register_batch_source("dataset_items", src).unwrap();
        let mut sources = SqlSources::new();
        sources.register_catalog_batches(&catalog);
        (catalog, sources)
    }

    #[test]
    fn sql_check_validates_params_and_unique_names_without_executing() {
        let (catalog, _) = sample_catalog_and_sources();
        let mut params = Map::new();
        params.insert("min_id".to_owned(), Value::from(2));
        let cols = sql_check(
            &catalog,
            "SELECT id, label FROM dataset_items WHERE id >= {min_id:Int}",
            &params,
        )
        .unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].type_name(), "integer");
        assert_eq!(cols[1].name, "label");
        assert_eq!(cols[1].type_name(), "string");

        // Usage-site coercion failure returns InvalidRequest
        let mut bad_str = Map::new();
        bad_str.insert("s".to_owned(), Value::from("not-an-int"));
        let err = sql_check(
            &catalog,
            "SELECT id FROM dataset_items WHERE id = {s:String}",
            &bad_str,
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::InvalidRequest(_)), "{err:?}");

        // Duplicate output names fail unique_names
        let dup = sql_check(&catalog, "SELECT id, id FROM dataset_items", &Map::new()).unwrap_err();
        assert!(matches!(dup, QueryError::Invalid(_)), "{dup:?}");
    }

    #[test]
    fn sql_json_truncation_limit_and_exports_work() {
        let (catalog, sources) = sample_catalog_and_sources();
        let (ctx, _dir) = test_ctx(1024 * 1024);

        // Limit 0 rejected
        let err0 = sql_json(&catalog, &sources, &ctx, "SELECT 1", &Map::new(), 0).unwrap_err();
        assert!(matches!(err0, QueryError::InvalidRequest(_)));

        // Truncated at limit 3 out of 5 rows
        let res = sql_json(
            &catalog,
            &sources,
            &ctx,
            "SELECT id, label FROM dataset_items ORDER BY id",
            &Map::new(),
            3,
        )
        .unwrap();
        assert!(res.truncated);
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0]["id"], Value::from(1));
        assert_eq!(res.rows[2]["id"], Value::from(3));
        assert_eq!(ctx.pool.reserved(), 0);

        // Exact fit (5 rows, limit 5) -> truncated = false
        let exact = sql_json(
            &catalog,
            &sources,
            &ctx,
            "SELECT id, label FROM dataset_items ORDER BY id",
            &Map::new(),
            5,
        )
        .unwrap();
        assert!(!exact.truncated);
        assert_eq!(exact.rows.len(), 5);
        assert_eq!(ctx.pool.reserved(), 0);

        // CSV streaming export
        let (schema, mut stream) = sql_export_stream(
            &catalog,
            &sources,
            &ctx,
            "SELECT id, label FROM dataset_items ORDER BY id",
            &Map::new(),
        )
        .unwrap();
        let mut csv_bytes = Vec::new();
        export_csv_stream(
            &schema,
            stream.as_mut(),
            &ctx,
            EXPORT_PIECE_BYTES,
            EXPORT_MAX_CHUNK_BYTES,
            |chunk| {
                csv_bytes.extend_from_slice(&chunk);
                Ok(())
            },
        )
        .unwrap();
        drop(stream);
        let csv_text = String::from_utf8(csv_bytes).unwrap();
        assert_eq!(
            csv_text,
            "id,label\r\n1,a\r\n2,'=1+1\r\n3,c\r\n4,d\r\n5,e\r\n"
        );
        assert_eq!(ctx.pool.reserved(), 0);

        // NDJSON streaming export
        let (_, mut nd_stream) = sql_export_stream(
            &catalog,
            &sources,
            &ctx,
            "SELECT id, label FROM dataset_items ORDER BY id",
            &Map::new(),
        )
        .unwrap();
        let mut nd_bytes = Vec::new();
        export_ndjson_stream(
            nd_stream.as_mut(),
            &ctx,
            EXPORT_PIECE_BYTES,
            EXPORT_MAX_CHUNK_BYTES,
            |chunk| {
                nd_bytes.extend_from_slice(&chunk);
                Ok(())
            },
        )
        .unwrap();
        drop(nd_stream);
        let nd_text = String::from_utf8(nd_bytes).unwrap();
        assert_eq!(
            nd_text,
            "{\"id\":1,\"label\":\"a\"}\n{\"id\":2,\"label\":\"=1+1\"}\n{\"id\":3,\"label\":\"c\"}\n{\"id\":4,\"label\":\"d\"}\n{\"id\":5,\"label\":\"e\"}\n"
        );
        assert_eq!(ctx.pool.reserved(), 0);
    }
}
