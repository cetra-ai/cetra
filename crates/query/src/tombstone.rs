//! Unified tombstone predicate builder and batch filter (D-A1-3).
//!
//! Matches `crates/storage/src/deletion.rs` on `key_columns`, `covered_predicate`,
//! `filter_uncovered` (`drop_covered`), and GDPR redaction (`"{col} IN (<{n} redacted>)"`).

use std::fmt;

use cetra_column::{Batch, DataType, Reservation, ScalarValue, Schema};

use crate::eval::{eval_filter, pool_size, reconcile_output, rollback_to, try_reserve};
use crate::{BinaryOp, EvalContext, Expr, QueryError, UnaryOp};

const COL_TRACE_ID: &str = "trace_id";
const COL_USER_ID: &str = "user_id";
const COL_SESSION_ID: &str = "session_id";
const COL_INGEST_TIME: &str = "ingest_time";

/// Physical table kind for tombstone matching (mirrors `cetra_storage::catalog::FileTable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneTable {
    /// `spans`: matches `trace_id` and `user_id`, gated by `ingest_time <= cutoff`.
    Spans,
    /// `scores`: matches `trace_id`, `user_id`, and `session_id`, gated by `ingest_time <= cutoff`.
    Scores,
    /// `traces`: matches `trace_id` only (no `ingest_time` gate).
    Traces,
}

/// Minimal view of one pending deletion needed to build the unified tombstone predicate (D-A1-3).
#[derive(Clone)]
pub struct TombstoneSpec {
    /// Inclusive `ingest_time` cutoff (UTC ns).
    pub cutoff: i64,
    /// Erased 16-byte trace IDs (sorted and deduplicated by `covered_predicate`).
    pub trace_ids: Vec<[u8; 16]>,
    /// Erased user IDs (sorted, deduplicated, and redacted in `Display`/`Debug`).
    pub users: Vec<String>,
    /// Erased session IDs (matched on `Scores` only; sorted, deduplicated, and redacted in `Display`/`Debug`).
    pub sessions: Vec<String>,
}

impl fmt::Debug for TombstoneSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TombstoneSpec")
            .field("cutoff", &self.cutoff)
            .field("trace_ids_len", &self.trace_ids.len())
            .field("users_len", &self.users.len())
            .field("sessions_len", &self.sessions.len())
            .finish()
    }
}

/// Required columns for evaluating tombstones on `table` (stored schemas).
pub fn key_columns(table: TombstoneTable) -> &'static [&'static str] {
    match table {
        TombstoneTable::Spans => &[COL_TRACE_ID, COL_USER_ID, COL_INGEST_TIME],
        TombstoneTable::Scores => &[COL_TRACE_ID, COL_USER_ID, COL_SESSION_ID, COL_INGEST_TIME],
        TombstoneTable::Traces => &[COL_TRACE_ID],
    }
}

/// Builds one null-safe boolean [`Expr`] matching rows covered by any deletion in `deletions`.
///
/// Runs against **stored** table schemas (`trace_id: FixedBytes16`, `user_id: Utf8`,
/// `session_id: Utf8`, `ingest_time: TimestampNs` — preserving `ingest_time`'s `utc` flag from `schema`).
/// Returns `Err(QueryError::Failed(_))` if a required column is missing or has an unexpected `DataType`.
/// Every `IN` list is sorted & deduplicated and guarded by `col IS NOT NULL AND col IN (...)`.
/// `user_id` and `session_id` `InList` nodes carry `redacted: true` so `fmt_plan()` and `Debug`
/// print `"{col} IN (<{n} redacted>)"`.
/// Returns `Ok(None)` when no deletion produces an active term for `table`.
pub fn covered_predicate(
    table: TombstoneTable,
    schema: &Schema,
    deletions: &[TombstoneSpec],
) -> Result<Option<Expr>, QueryError> {
    let mut covered: Option<Expr> = None;
    for deletion in deletions {
        let mut terms: Vec<Expr> = Vec::new();
        if !deletion.trace_ids.is_empty() {
            let mut ids = deletion.trace_ids.clone();
            ids.sort_unstable();
            ids.dedup();
            let index = resolve_fixed_bytes16(schema, COL_TRACE_ID)?;
            let col = Expr::Column {
                index,
                name: COL_TRACE_ID.to_owned(),
                data_type: DataType::FixedBytes16,
            };
            let list = ids
                .into_iter()
                .map(|id| Expr::Literal(ScalarValue::FixedBytes16(id)))
                .collect();
            terms.push(guarded_in_list(col, list, false));
        }
        if table != TombstoneTable::Traces && !deletion.users.is_empty() {
            let mut users: Vec<&str> = deletion.users.iter().map(String::as_str).collect();
            users.sort_unstable();
            users.dedup();
            terms.push(redacted_utf8_term(schema, COL_USER_ID, &users)?);
        }
        if table == TombstoneTable::Scores && !deletion.sessions.is_empty() {
            let mut sessions: Vec<&str> = deletion.sessions.iter().map(String::as_str).collect();
            sessions.sort_unstable();
            sessions.dedup();
            terms.push(redacted_utf8_term(schema, COL_SESSION_ID, &sessions)?);
        }
        let Some(mut expression) = or_together(terms) else {
            continue;
        };
        if table != TombstoneTable::Traces {
            let (index, utc) = resolve_timestamp(schema, COL_INGEST_TIME)?;
            let ingest = Expr::Column {
                index,
                name: COL_INGEST_TIME.to_owned(),
                data_type: DataType::TimestampNs { utc },
            };
            let cutoff = Expr::Literal(ScalarValue::TimestampNs {
                value: deletion.cutoff,
                utc,
            });
            let within = Expr::Binary {
                left: Box::new(ingest),
                op: BinaryOp::LtEq,
                right: Box::new(cutoff),
                data_type: DataType::Bool,
            };
            expression = Expr::Binary {
                left: Box::new(within),
                op: BinaryOp::And,
                right: Box::new(expression),
                data_type: DataType::Bool,
            };
        }
        covered = Some(match covered {
            None => expression,
            Some(existing) => Expr::Binary {
                left: Box::new(existing),
                op: BinaryOp::Or,
                right: Box::new(expression),
                data_type: DataType::Bool,
            },
        });
    }
    Ok(covered)
}

/// Drops rows matched by `predicate` from `batch` (updating `batch`'s `Selection` mask without copying payloads).
///
/// `Reservation` postcondition: rolls back to `before` on `Err`, and on `Ok(out)` reconciles `pool`
/// to `before + out.selection().memory_bytes()` (after dropping temporary `covered` / inverted masks).
pub fn filter_uncovered(
    batch: &Batch,
    predicate: &Expr,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let before = pool_size(&pool);
    let covered = match eval_filter(predicate, batch, ctx, pool.as_deref_mut()) {
        Ok(bm) => bm,
        Err(err) => {
            rollback_to(&mut pool, before);
            return Err(err);
        }
    };
    let covered_bytes = covered.memory_bytes();
    if let Err(err) = try_reserve(&mut pool, covered_bytes) {
        rollback_to(&mut pool, before);
        return Err(err);
    }
    let keep = covered.not();
    drop(covered);
    if let Some(res) = pool.as_deref_mut() {
        res.shrink(covered_bytes);
    }
    let out = match batch.filter_mask(&keep) {
        Ok(b) => b,
        Err(err) => {
            rollback_to(&mut pool, before);
            return Err(err.into());
        }
    };
    drop(keep);
    reconcile_output(&mut pool, before, out.selection().memory_bytes())?;
    Ok(out)
}

fn redacted_utf8_term(schema: &Schema, column: &str, values: &[&str]) -> Result<Expr, QueryError> {
    let index = resolve_utf8(schema, column)?;
    let col = Expr::Column {
        index,
        name: column.to_owned(),
        data_type: DataType::Utf8,
    };
    let list = values
        .iter()
        .map(|value| Expr::Literal(ScalarValue::Utf8((*value).to_owned())))
        .collect();
    Ok(guarded_in_list(col, list, true))
}

fn guarded_in_list(col: Expr, list: Vec<Expr>, redacted: bool) -> Expr {
    let guard = Expr::Unary {
        op: UnaryOp::IsNotNull,
        expr: Box::new(col.clone()),
        data_type: DataType::Bool,
    };
    let membership = Expr::InList {
        expr: Box::new(col),
        list,
        negated: false,
        redacted,
    };
    Expr::Binary {
        left: Box::new(guard),
        op: BinaryOp::And,
        right: Box::new(membership),
        data_type: DataType::Bool,
    }
}

fn or_together(terms: Vec<Expr>) -> Option<Expr> {
    let mut iter = terms.into_iter();
    let first = iter.next()?;
    Some(iter.fold(first, |acc, term| Expr::Binary {
        left: Box::new(acc),
        op: BinaryOp::Or,
        right: Box::new(term),
        data_type: DataType::Bool,
    }))
}

fn column_index(schema: &Schema, name: &str) -> Result<usize, QueryError> {
    schema
        .index_of(name)
        .ok_or_else(|| QueryError::failed(format!("{name} is missing from the scan schema")))
}

fn resolve_fixed_bytes16(schema: &Schema, name: &str) -> Result<usize, QueryError> {
    let idx = column_index(schema, name)?;
    let dt = &schema.fields()[idx].data_type;
    if *dt != DataType::FixedBytes16 {
        return Err(QueryError::failed(format!(
            "{name} is {}, not fixed 16-byte binary",
            dt.display_name()
        )));
    }
    Ok(idx)
}

fn resolve_utf8(schema: &Schema, name: &str) -> Result<usize, QueryError> {
    let idx = column_index(schema, name)?;
    let dt = &schema.fields()[idx].data_type;
    if *dt != DataType::Utf8 {
        return Err(QueryError::failed(format!(
            "{name} is {}, not string",
            dt.display_name()
        )));
    }
    Ok(idx)
}

fn resolve_timestamp(schema: &Schema, name: &str) -> Result<(usize, bool), QueryError> {
    let idx = column_index(schema, name)?;
    match schema.fields()[idx].data_type {
        DataType::TimestampNs { utc } => Ok((idx, utc)),
        ref other => Err(QueryError::failed(format!(
            "{name} is {}, not a timestamp",
            other.display_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        Bitmap, CancelToken, Column, Field, FlatColumn, FlatData, GlobalPool, Selection, Utf8Data,
    };

    use super::*;
    use crate::eval::eval_expr;

    fn ctx() -> EvalContext {
        EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        }
    }

    fn tid(b: u8) -> [u8; 16] {
        [b; 16]
    }

    fn spans_stored_schema(utc: bool) -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("ingest_time", DataType::TimestampNs { utc }, false),
        ])
        .unwrap()
    }

    fn scores_stored_schema(utc: bool) -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, true),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("session_id", DataType::Utf8, true),
            Field::new("ingest_time", DataType::TimestampNs { utc }, false),
        ])
        .unwrap()
    }

    fn traces_stored_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![Field::new("trace_id", DataType::FixedBytes16, false)]).unwrap()
    }

    fn eval_bools(expr: &Expr, batch: &Batch) -> Vec<Option<bool>> {
        let col = eval_expr(expr, batch, &ctx(), None).unwrap();
        (0..batch.raw_len())
            .map(|r| match col.get_scalar(r).unwrap() {
                ScalarValue::Bool(b) => Some(b),
                ScalarValue::Null(_) => None,
                other => panic!("unexpected scalar {other:?}"),
            })
            .collect()
    }

    #[test]
    fn covered_predicate_null_safety_dedup_and_redaction() {
        assert_eq!(
            key_columns(TombstoneTable::Spans),
            &["trace_id", "user_id", "ingest_time"]
        );
        assert_eq!(
            key_columns(TombstoneTable::Scores),
            &["trace_id", "user_id", "session_id", "ingest_time"]
        );
        assert_eq!(key_columns(TombstoneTable::Traces), &["trace_id"]);

        // Empty deletions -> Ok(None)
        let spans_schema = spans_stored_schema(true);
        assert!(
            covered_predicate(TombstoneTable::Spans, &spans_schema, &[])
                .unwrap()
                .is_none()
        );

        // Traces ignores users/sessions -> Ok(None) when trace_ids is empty
        let traces_schema = traces_stored_schema();
        let user_only = TombstoneSpec {
            cutoff: 100,
            trace_ids: vec![],
            users: vec!["alice@secret.test".to_owned()],
            sessions: vec!["sess-secret".to_owned()],
        };
        assert!(
            covered_predicate(TombstoneTable::Traces, &traces_schema, &[user_only])
                .unwrap()
                .is_none()
        );

        // Missing or wrong-type column -> Err(QueryError::Failed(_))
        let missing_ingest = Schema::try_new(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("user_id", DataType::Utf8, true),
        ])
        .unwrap();
        let spec_trace = TombstoneSpec {
            cutoff: 100,
            trace_ids: vec![tid(1)],
            users: vec![],
            sessions: vec![],
        };
        assert!(matches!(
            covered_predicate(
                TombstoneTable::Spans,
                &missing_ingest,
                std::slice::from_ref(&spec_trace)
            ),
            Err(QueryError::Failed(_))
        ));

        let view_like_schema = Schema::try_new(vec![
            Field::new("trace_id", DataType::Utf8, false),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("ingest_time", DataType::timestamp_utc(), false),
        ])
        .unwrap();
        assert!(matches!(
            covered_predicate(
                TombstoneTable::Spans,
                &view_like_schema,
                std::slice::from_ref(&spec_trace)
            ),
            Err(QueryError::Failed(_))
        ));

        let wrong_ingest = Schema::try_new(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("ingest_time", DataType::I64, false),
        ])
        .unwrap();
        assert!(matches!(
            covered_predicate(TombstoneTable::Spans, &wrong_ingest, &[spec_trace]),
            Err(QueryError::Failed(_))
        ));

        // Duplicate users & sessions are sorted and deduplicated, and never leaked in fmt_plan / Debug.
        let secret_user_a = "alice_erased@example.com";
        let secret_user_b = "bob_erased@example.com";
        let secret_sess = "sess_erased_999";
        let dup_spec = TombstoneSpec {
            cutoff: 1_000,
            trace_ids: vec![tid(2), tid(1), tid(2)],
            users: vec![
                secret_user_b.to_owned(),
                secret_user_a.to_owned(),
                secret_user_b.to_owned(),
            ],
            sessions: vec![secret_sess.to_owned(), secret_sess.to_owned()],
        };
        let spec_dbg = format!("{dup_spec:?}");
        assert!(!spec_dbg.contains(secret_user_a), "leaked in {spec_dbg}");
        assert!(!spec_dbg.contains(secret_sess), "leaked in {spec_dbg}");

        let scores_schema = scores_stored_schema(false);
        let pred = covered_predicate(
            TombstoneTable::Scores,
            &scores_schema,
            std::slice::from_ref(&dup_spec),
        )
        .unwrap()
        .unwrap();
        let plan = pred.fmt_plan();
        let dbg = format!("{pred:?}");
        assert!(plan.contains("user_id IN (<2 redacted>)"), "{plan}");
        assert!(plan.contains("session_id IN (<1 redacted>)"), "{plan}");
        assert!(dbg.contains("<2 redacted>"), "{dbg}");
        assert!(dbg.contains("<1 redacted>"), "{dbg}");
        assert!(!plan.contains(secret_user_a), "{plan}");
        assert!(!plan.contains(secret_user_b), "{plan}");
        assert!(!plan.contains(secret_sess), "{plan}");
        assert!(!dbg.contains(secret_user_a), "{dbg}");
        assert!(!dbg.contains(secret_user_b), "{dbg}");
        assert!(!dbg.contains(secret_sess), "{dbg}");

        // Evaluate Scores predicate: nullable trace_id/user_id/session_id never produce NULL,
        // cutoff boundary (`<=` covered, `>` kept), and utc=false preserved.
        let scores_batch = Batch::try_new_all(
            Arc::clone(&scores_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![tid(1), tid(9), tid(9), tid(9), tid(1)]),
                        Some(Bitmap::from_bools(&[true, false, true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "other",
                                secret_user_a,
                                "other",
                                "other",
                                "",
                            ])
                            .unwrap(),
                        ),
                        Some(Bitmap::from_bools(&[true, true, false, false, false])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "other",
                                "other",
                                secret_sess,
                                "other",
                                "other",
                            ])
                            .unwrap(),
                        ),
                        Some(Bitmap::from_bools(&[true, true, true, false, false])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![1_000, 500, 1_000, 500, 1_001],
                            utc: false,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            5,
        )
        .unwrap();
        // Row 0: trace_id=tid(1), ingest=1000 <= 1000 -> covered (true)
        // Row 1: trace_id=NULL, user_id=secret_user_a, ingest=500 -> covered (true)
        // Row 2: trace_id=tid(9), user_id=NULL, session_id=secret_sess, ingest=1000 -> covered (true)
        // Row 3: all ids NULL/miss, ingest=500 -> false (never NULL!)
        // Row 4: trace_id=tid(1), ingest=1001 > 1000 -> false
        assert_eq!(
            eval_bools(&pred, &scores_batch),
            vec![Some(true), Some(true), Some(true), Some(false), Some(false),]
        );

        // `filter_uncovered` keeps rows 3 and 4, respects existing Selection, and reconciles Reservation.
        let pool = GlobalPool::standalone(1024 * 1024);
        let mut res = pool.register("tombstone", true);
        res.try_grow(100).unwrap();
        let filtered = filter_uncovered(&scores_batch, &pred, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(res.size(), 100 + filtered.selection().memory_bytes());
        assert_eq!(filtered.num_rows(), 2);
        assert_eq!(
            filtered.selection().to_indices(filtered.raw_len()).unwrap(),
            vec![3, 4]
        );

        // Existing Selection::Indices keeps only uncovered active indices.
        let before_idx = res.size();
        let indexed_batch = scores_batch
            .with_selection(Selection::Indices(vec![0, 3]))
            .unwrap();
        let filtered_idx = filter_uncovered(&indexed_batch, &pred, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(
            res.size(),
            before_idx + filtered_idx.selection().memory_bytes()
        );
        assert_eq!(
            filtered_idx
                .selection()
                .to_indices(filtered_idx.raw_len())
                .unwrap(),
            vec![3]
        );

        // On Traces: no ingest_time gate, sessions/users ignored.
        let traces_batch = Batch::try_new_all(
            Arc::clone(&traces_schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::FixedBytes16(vec![tid(1), tid(3)]), None).unwrap(),
            )],
            2,
        )
        .unwrap();
        let traces_pred = covered_predicate(
            TombstoneTable::Traces,
            &traces_schema,
            std::slice::from_ref(&dup_spec),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            eval_bools(&traces_pred, &traces_batch),
            vec![Some(true), Some(false)]
        );

        // On Spans: users matched, sessions ignored, ingest_time <= cutoff gated.
        let spans_batch = Batch::try_new_all(
            Arc::clone(&spans_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::FixedBytes16(vec![tid(9), tid(9), tid(9)]), None)
                        .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([secret_user_a, "other", secret_user_a])
                                .unwrap(),
                        ),
                        Some(Bitmap::from_bools(&[true, true, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![1_000, 500, 1_001],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();
        let spans_pred = covered_predicate(TombstoneTable::Spans, &spans_schema, &[dup_spec])
            .unwrap()
            .unwrap();
        assert_eq!(
            eval_bools(&spans_pred, &spans_batch),
            vec![Some(true), Some(false), Some(false)]
        );

        // Error paths in `filter_uncovered` roll back `Reservation` to `before`.
        let mut res_err = pool.register("tombstone_err", true);
        res_err.try_grow(64).unwrap();
        let non_bool = Expr::Literal(ScalarValue::I64(1));
        assert!(matches!(
            filter_uncovered(&scores_batch, &non_bool, &ctx(), Some(&mut res_err)),
            Err(QueryError::Failed(_))
        ));
        assert_eq!(res_err.size(), 64);

        let tiny_pool = GlobalPool::standalone(32);
        let mut tiny_res = tiny_pool.register("tombstone_oom", true);
        tiny_res.try_grow(30).unwrap();
        assert!(matches!(
            filter_uncovered(&scores_batch, &pred, &ctx(), Some(&mut tiny_res)),
            Err(QueryError::TooLarge)
        ));
        assert_eq!(tiny_res.size(), 30);

        // Reservation must cover the temporary inverted `keep = covered.not()` mask
        // even when `eval_filter` itself fits in the pool.
        let large_rows = 8192usize;
        let bool_schema =
            Schema::try_new_arc(vec![Field::new("flag", DataType::Bool, false)]).unwrap();
        let const_flag = Column::try_new_const(ScalarValue::Bool(true), large_rows).unwrap();
        let large_batch = Batch::try_new_all(bool_schema, vec![const_flag], large_rows).unwrap();
        let const_pred = Expr::Literal(ScalarValue::Bool(true));
        // Bitmap for 8192 rows is 1024 bytes; eval_filter peaks at 1024 bytes,
        // and covered.not() needs another 1024 bytes (total 2048).
        let inv_pool = GlobalPool::standalone(1536);
        let mut inv_res = inv_pool.register("tombstone_inv_oom", true);
        assert!(matches!(
            filter_uncovered(&large_batch, &const_pred, &ctx(), Some(&mut inv_res)),
            Err(QueryError::TooLarge)
        ));
        assert_eq!(inv_res.size(), 0);
    }
}
