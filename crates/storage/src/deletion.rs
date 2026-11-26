//! Deletion requests, their tombstone set, and the covered-row rule (spec 03 §7).
//!
//! [`cetra_query::tombstone::covered_predicate`] is the one place the
//! covered-row rule exists (D-A1-3): queries, flush and compaction all use it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use cetra_core::ids::{ProjectId, TraceId};
#[cfg(test)]
use cetra_core::schema::spans;
use chrono::NaiveDate;
use uuid::Uuid;

use crate::error::{EngineError, QueryError};
use crate::memtable::{MemBatch, MemSnapshot, PartitionKey};
use crate::wal::Table;

/// One deletion request handed over by the server (spec 03 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deletion {
    /// The server's `meta.db` id, UUIDv7. Registration is idempotent by it.
    pub id: Uuid,
    /// The project whose data the deletion covers.
    pub project: ProjectId,
    /// Rows with `ingest_time` at or before this (ns) are covered.
    pub cutoff: i64,
    /// Which rows the deletion covers.
    pub scope: DeletionScope,
}

/// Which rows a deletion covers (spec 03 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletionScope {
    /// The rows of the listed traces.
    Traces {
        /// Covered trace ids.
        trace_ids: Vec<TraceId>,
    },
    /// The rows of one user: their trace ids, their user id and, on `scores`
    /// only, their session ids.
    User {
        /// The user the deletion is about.
        user_id: String,
        /// Covered trace ids.
        trace_ids: Vec<TraceId>,
        /// Covered session ids; they match `scores` rows only.
        session_ids: Vec<String>,
    },
}

/// How far a deletion has progressed (spec 03 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionStatus {
    /// Registered; its rows are hidden by tombstones.
    Pending,
    /// Every file and memtable batch that could hold a covered row has been
    /// rewritten or checked, at `applied_at` (ns).
    Applied {
        /// When the deletion was marked applied, ns.
        applied_at: i64,
    },
}

/// A pending deletion as the read path and the jobs use it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PendingDeletion {
    pub(crate) id: Uuid,
    pub(crate) seq: u64,
    pub(crate) project: ProjectId,
    pub(crate) cutoff: i64,
    pub(crate) registered_at: i64,
    pub(crate) trace_ids: HashSet<[u8; 16]>,
    /// Empty for a `Traces` scope, one entry for a `User` scope.
    pub(crate) users: HashSet<String>,
    /// Empty for a `Traces` scope.
    pub(crate) sessions: HashSet<String>,
}

impl std::fmt::Debug for PendingDeletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingDeletion")
            .field("id", &self.id)
            .field("seq", &self.seq)
            .field("project", &self.project)
            .field("cutoff", &self.cutoff)
            .field("registered_at", &self.registered_at)
            .field("trace_ids_len", &self.trace_ids.len())
            .field("users_len", &self.users.len())
            .field("sessions_len", &self.sessions.len())
            .finish()
    }
}

impl PendingDeletion {
    /// Converts this pending deletion into a `cetra_query::TombstoneSpec`.
    pub(crate) fn to_tombstone_spec(&self) -> cetra_query::TombstoneSpec {
        cetra_query::TombstoneSpec {
            cutoff: self.cutoff,
            trace_ids: self.trace_ids.iter().copied().collect(),
            users: self.users.iter().cloned().collect(),
            sessions: self.sessions.iter().cloned().collect(),
        }
    }
}

/// Converts a slice of [`PendingDeletion`]s into [`cetra_query::TombstoneSpec`]s.
pub(crate) fn to_tombstone_specs(
    deletions: &[Arc<PendingDeletion>],
) -> Vec<cetra_query::TombstoneSpec> {
    deletions.iter().map(|d| d.to_tombstone_spec()).collect()
}

/// Every pending deletion, by project, plus the known sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tombstones {
    known_seq: u64,
    by_project: HashMap<ProjectId, Arc<[Arc<PendingDeletion>]>>,
}

impl Tombstones {
    /// The highest `deletion_seq` registered so far, pending or applied, or 0.
    pub(crate) fn known_seq(&self) -> u64 {
        self.known_seq
    }

    /// The project's pending deletions in `seq` order; empty when it has none.
    pub(crate) fn for_project(&self, project: ProjectId) -> Arc<[Arc<PendingDeletion>]> {
        self.by_project
            .get(&project)
            .map_or_else(|| Arc::from(Vec::new()), Arc::clone)
    }

    /// Every pending deletion, in `seq` order.
    pub(crate) fn all(&self) -> Vec<Arc<PendingDeletion>> {
        let mut all: Vec<_> = self
            .by_project
            .values()
            .flat_map(|deletions| deletions.iter().map(Arc::clone))
            .collect();
        all.sort_by_key(|deletion| deletion.seq);
        all
    }

    /// A copy with `deletion` added and `known_seq = max(known_seq, deletion.seq)`.
    ///
    /// The project's slice stays sorted by `seq`.
    pub(crate) fn with(&self, deletion: PendingDeletion) -> Tombstones {
        let project = deletion.project;
        let seq = deletion.seq;
        let mut by_project = self.by_project.clone();
        let mut list: Vec<Arc<PendingDeletion>> = by_project
            .remove(&project)
            .map(|slice| slice.iter().cloned().collect())
            .unwrap_or_default();
        let position = list.partition_point(|existing| existing.seq <= seq);
        list.insert(position, Arc::new(deletion));
        by_project.insert(project, list.into());
        Tombstones {
            known_seq: self.known_seq.max(seq),
            by_project,
        }
    }

    /// A copy without the deletion `id` of `project`. `known_seq` is unchanged.
    ///
    /// A project whose last deletion goes is removed from the map.
    pub(crate) fn without(&self, project: ProjectId, id: Uuid) -> Tombstones {
        let mut by_project = self.by_project.clone();
        if let Some(slice) = by_project.remove(&project) {
            let mut list: Vec<Arc<PendingDeletion>> = slice.iter().cloned().collect();
            list.retain(|deletion| deletion.id != id);
            if !list.is_empty() {
                by_project.insert(project, list.into());
            }
        }
        Tombstones {
            known_seq: self.known_seq,
            by_project,
        }
    }

    /// A copy with `known_seq = max(known_seq, seq)` and the same deletions
    /// (M5.7: a deletion registered for a dropped project).
    pub(crate) fn with_known_seq(&self, seq: u64) -> Tombstones {
        Tombstones {
            known_seq: self.known_seq.max(seq),
            by_project: self.by_project.clone(),
        }
    }

    /// A copy without any deletion of `project` (M5.7). `known_seq` is
    /// unchanged.
    pub(crate) fn without_project(&self, project: ProjectId) -> Tombstones {
        let mut by_project = self.by_project.clone();
        by_project.remove(&project);
        Tombstones {
            known_seq: self.known_seq,
            by_project,
        }
    }

    /// The tombstones loaded from `catalog.db`: `deletions` in any order, under
    /// the `known_seq` read beside them (the highest sequence overall, pending
    /// or applied). Each project's slice is sorted by `seq`.
    pub(crate) fn loaded(known_seq: u64, deletions: Vec<PendingDeletion>) -> Tombstones {
        let mut by_project: HashMap<ProjectId, Vec<Arc<PendingDeletion>>> = HashMap::new();
        for deletion in deletions {
            by_project
                .entry(deletion.project)
                .or_default()
                .push(Arc::new(deletion));
        }
        Tombstones {
            known_seq,
            by_project: by_project
                .into_iter()
                .map(|(project, mut list)| {
                    list.sort_by_key(|deletion| deletion.seq);
                    (project, list.into())
                })
                .collect(),
        }
    }
}

/// The deletions that may cover rows of a file or batch: those with `seq`
/// above `applied` (`None` for a memtable batch) and `cutoff` at or after
/// `min_ingest_time` (spec 03 §7).
pub(crate) fn applicable(
    deletions: &[Arc<PendingDeletion>],
    applied: Option<u64>,
    min_ingest_time: i64,
) -> Vec<Arc<PendingDeletion>> {
    deletions
        .iter()
        .filter(|deletion| {
            applied.is_none_or(|applied| deletion.seq > applied)
                && deletion.cutoff >= min_ingest_time
        })
        .cloned()
        .collect()
}

/// The rows of `batch` not covered by `predicate`, materialized via [`cetra_column::Batch::compact`].
pub(crate) fn drop_covered_batch(
    batch: &cetra_column::Batch,
    predicate: &cetra_query::Expr,
) -> Result<cetra_column::Batch, cetra_query::QueryError> {
    let ctx = cetra_query::EvalContext {
        now_ns: crate::catalog::now_ns(),
        cancel: cetra_column::CancelToken::new(),
    };
    let filtered = cetra_query::tombstone::filter_uncovered(batch, predicate, &ctx, None)?;
    Ok(filtered.compact(None)?)
}

/// A filtered copy of `snapshot` for `project`: every batch without its covered
/// rows, other projects' partitions left out, and the partitions of days before
/// the project's retention watermark dropped entirely (spec 03 §8). Batches no
/// deletion can cover are kept as they are, sharing their `Arc`s and partials
/// cache.
pub(crate) fn project_snapshot(
    table: Table,
    snapshot: &MemSnapshot,
    project: ProjectId,
    keep_from: Option<NaiveDate>,
    deletions: &[Arc<PendingDeletion>],
) -> Result<Arc<MemSnapshot>, QueryError> {
    let tombstone_table = match table {
        Table::Spans => cetra_query::TombstoneTable::Spans,
        Table::Scores => cetra_query::TombstoneTable::Scores,
    };
    let mut partitions: BTreeMap<PartitionKey, Vec<MemBatch>> = BTreeMap::new();
    for (key, batches) in &snapshot.partitions {
        if key.project != project {
            continue;
        }
        if keep_from.is_some_and(|keep| key.day < keep) {
            continue;
        }
        let mut kept = Vec::with_capacity(batches.len());
        for batch in batches {
            let covering = applicable(deletions, None, batch.min_ingest_time);
            if covering.is_empty() {
                kept.push(batch.clone());
                continue;
            }
            let specs = to_tombstone_specs(&covering);
            let Some(predicate) = cetra_query::tombstone::covered_predicate(
                tombstone_table,
                batch.batch.schema(),
                &specs,
            )
            .map_err(|err| QueryError::Internal(EngineError::from_query(&err)))?
            else {
                kept.push(batch.clone());
                continue;
            };
            let filtered = drop_covered_batch(&batch.batch, &predicate)
                .map_err(|err| QueryError::Internal(EngineError::from_query(&err)))?;
            if filtered.num_rows() == 0 {
                continue;
            }
            let rebuilt = MemBatch::from_batch(table, filtered).map_err(|_| {
                QueryError::Internal(EngineError::internal("memtable rebuild failed"))
            })?;
            kept.push(rebuilt);
        }
        if !kept.is_empty() {
            partitions.insert(*key, kept);
        }
    }
    Ok(Arc::new(MemSnapshot { partitions }))
}

#[cfg(test)]
mod tests {
    use cetra_column::{Batch, CancelToken, Column, ScalarValue, Selection};
    use cetra_core::schema::{
        scores_schema as core_scores_schema, spans_schema as core_spans_schema,
        traces_schema as core_traces_schema,
    };
    use cetra_core::score::ScoreTarget;
    use cetra_query::{EvalContext, Expr, TombstoneTable, covered_predicate, eval_expr};

    use super::*;
    use crate::test_support::{score_row, score_rows, span_row, span_rows, trace_batch};

    fn project(which: u8) -> ProjectId {
        format!("018f6b4e-7c1a-7c1a-8c1a-0000000000{which:02x}")
            .parse()
            .unwrap()
    }

    fn pending(project: ProjectId, seq: u64) -> PendingDeletion {
        PendingDeletion {
            id: Uuid::from_u128(u128::from(seq)),
            seq,
            project,
            cutoff: 1,
            registered_at: 2,
            trace_ids: HashSet::new(),
            users: HashSet::new(),
            sessions: HashSet::new(),
        }
    }

    /// A deletion of the first project, `seq` 1, with an explicit scope.
    fn scoped(
        traces: &[u16],
        users: &[&str],
        sessions: &[&str],
        cutoff: i64,
    ) -> Arc<PendingDeletion> {
        let trace_ids = traces
            .iter()
            .map(|trace| *span_row(*trace, 0, 0, 0).trace_id.as_bytes())
            .collect();
        Arc::new(PendingDeletion {
            id: Uuid::from_u128(1),
            seq: 1,
            project: project(1),
            cutoff,
            registered_at: 2,
            trace_ids,
            users: users.iter().map(|user| (*user).to_owned()).collect(),
            sessions: sessions
                .iter()
                .map(|session| (*session).to_owned())
                .collect(),
        })
    }

    /// The predicate's value for every row of `batch`: `None` is a null.
    fn values_of(predicate: &Expr, batch: &Batch) -> Vec<Option<bool>> {
        let ctx = EvalContext {
            now_ns: crate::catalog::now_ns(),
            cancel: CancelToken::new(),
        };
        let col = eval_expr(predicate, batch, &ctx, None).unwrap();
        (0..batch.num_rows())
            .map(|row| match col.get_scalar(row).unwrap() {
                ScalarValue::Null(_) => None,
                ScalarValue::Bool(b) => Some(b),
                other => panic!("expected bool or null, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn with_keeps_seq_order() {
        let one = project(1);
        let tombstones = Tombstones::default()
            .with(pending(one, 3))
            .with(pending(one, 1))
            .with(pending(one, 2));
        let seqs: Vec<_> = tombstones
            .for_project(one)
            .iter()
            .map(|deletion| deletion.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(tombstones.known_seq(), 3);
        let all: Vec<_> = tombstones
            .all()
            .iter()
            .map(|deletion| deletion.seq)
            .collect();
        assert_eq!(all, vec![1, 2, 3]);
    }

    #[test]
    fn without_removes_and_drops_an_emptied_project() {
        let one = project(1);
        let two = project(2);
        let first = pending(one, 1);
        let id = first.id;
        let tombstones = Tombstones::default()
            .with(first)
            .with(pending(two, 2))
            .without(one, id);
        assert!(tombstones.for_project(one).is_empty());
        assert!(!tombstones.by_project.contains_key(&one));
        assert_eq!(tombstones.for_project(two).len(), 1);
        assert_eq!(tombstones.known_seq(), 2);
    }

    #[test]
    fn for_project_of_an_unknown_project_is_empty() {
        let tombstones = Tombstones::default().with(pending(project(1), 1));
        assert!(tombstones.for_project(project(2)).is_empty());
    }

    #[test]
    fn a_traces_deletion_covers_a_trace_up_to_the_cutoff() {
        let cutoff = 1_000;
        let batch = span_rows(&[
            span_row(1, 0, 10, cutoff),
            span_row(1, 1, 20, cutoff + 1),
            span_row(2, 0, 30, cutoff),
        ]);
        let specs = to_tombstone_specs(&[scoped(&[1], &[], &[], cutoff)]);
        let predicate = covered_predicate(TombstoneTable::Spans, batch.schema(), &specs)
            .unwrap()
            .unwrap();
        assert_eq!(
            values_of(&predicate, &batch),
            vec![Some(true), Some(false), Some(false)]
        );
    }

    #[test]
    fn a_user_deletion_covers_the_user_but_not_a_null_user_id() {
        let mut covered = span_row(1, 0, 10, 1);
        covered.user_id = Some("u".to_owned());
        let plain = span_row(2, 0, 20, 1);
        let batch = span_rows(&[covered, plain]);
        let specs = to_tombstone_specs(&[scoped(&[], &["u"], &[], 1_000)]);
        let predicate = covered_predicate(TombstoneTable::Spans, batch.schema(), &specs)
            .unwrap()
            .unwrap();
        // Trace 1 carries the user id, trace 2 does not. The guard keeps the
        // expression false, never null, on the null user id.
        assert_eq!(values_of(&predicate, &batch), vec![Some(true), Some(false)]);
    }

    #[test]
    fn session_ids_cover_scores_but_not_spans() {
        let mut covered = span_row(1, 0, 10, 1);
        covered.session_id = Some("s".to_owned());
        let mut other = span_row(2, 0, 20, 1);
        other.session_id = Some("s".to_owned());
        let spans_batch = span_rows(&[covered, other]);
        let mut score = score_row(0x11, 0, 1, 1, 1, false, "sess");
        score.target = ScoreTarget::Session {
            session_id: "s".to_owned(),
        };
        let scores_batch = score_rows(&[score]);
        let specs = to_tombstone_specs(&[scoped(&[], &["someone-else"], &["s"], 1_000)]);

        let on_scores = covered_predicate(TombstoneTable::Scores, scores_batch.schema(), &specs)
            .unwrap()
            .unwrap();
        assert_eq!(values_of(&on_scores, &scores_batch), vec![Some(true)]);

        // `session_id` matches `scores` only: both spans stay.
        let on_spans = covered_predicate(TombstoneTable::Spans, spans_batch.schema(), &specs)
            .unwrap()
            .unwrap();
        assert_eq!(
            values_of(&on_spans, &spans_batch),
            vec![Some(false), Some(false)]
        );
    }

    #[test]
    fn a_row_covered_by_either_of_two_deletions_is_covered() {
        let batch = span_rows(&[
            span_row(1, 0, 10, 1),
            span_row(2, 0, 20, 1),
            span_row(3, 0, 30, 1),
        ]);
        let specs =
            to_tombstone_specs(&[scoped(&[1], &[], &[], 1_000), scoped(&[2], &[], &[], 1_000)]);
        let predicate = covered_predicate(TombstoneTable::Spans, batch.schema(), &specs)
            .unwrap()
            .unwrap();
        assert_eq!(
            values_of(&predicate, &batch),
            vec![Some(true), Some(true), Some(false)]
        );
    }

    #[test]
    fn empty_deletions_build_no_predicate_and_traces_tests_only_the_trace_id() {
        let spans_schema = core_spans_schema();
        assert!(
            covered_predicate(TombstoneTable::Spans, &spans_schema, &[])
                .unwrap()
                .is_none()
        );
        // A partial carries no per-row ingest time: a row ingested after the
        // cut-off is still covered, and a user scope covers no partial at all.
        let traces = trace_batch(&[span_row(1, 0, 10, 1_000)]);
        let by_trace = to_tombstone_specs(&[scoped(&[1], &[], &[], 1)]);
        let predicate = covered_predicate(TombstoneTable::Traces, traces.schema(), &by_trace)
            .unwrap()
            .unwrap();
        assert_eq!(values_of(&predicate, &traces), vec![Some(true)]);
        let by_user = to_tombstone_specs(&[scoped(&[], &["u"], &["s"], 1_000)]);
        let traces_schema = core_traces_schema();
        assert!(
            covered_predicate(TombstoneTable::Traces, &traces_schema, &by_user)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_session_score_is_not_covered_by_a_traces_deletion() {
        let mut score = score_row(0x11, 0, 1, 1, 1, false, "sess");
        score.target = ScoreTarget::Session {
            session_id: "s".to_owned(),
        };
        let batch = score_rows(&[score]);
        let specs = to_tombstone_specs(&[scoped(&[1], &[], &[], 1_000)]);
        let scores_schema = core_scores_schema();
        let predicate = covered_predicate(TombstoneTable::Scores, &scores_schema, &specs)
            .unwrap()
            .unwrap();
        // A null `trace_id` makes the term false, not null.
        assert_eq!(values_of(&predicate, &batch), vec![Some(false)]);
    }

    #[test]
    fn drop_covered_batch_keeps_uncovered_rows_and_handles_selection_and_encodings() {
        let mut s1 = span_row(1, 0, 10, 1);
        s1.user_id = Some("alice".to_owned());
        let mut s2 = span_row(2, 0, 20, 1);
        s2.user_id = Some("bob".to_owned());
        let mut s3 = span_row(3, 0, 30, 1_000);
        s3.user_id = Some("alice".to_owned());

        let col_batch = span_rows(&[s1, s2, s3]);

        let del = scoped(&[1], &["bob"], &[], 1);
        let specs = to_tombstone_specs(std::slice::from_ref(&del));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].cutoff, 1);
        assert_eq!(specs[0].users, vec!["bob".to_owned()]);

        let pred = covered_predicate(TombstoneTable::Spans, col_batch.schema(), &specs)
            .unwrap()
            .unwrap();
        let kept = drop_covered_batch(&col_batch, &pred).unwrap();
        assert_eq!(kept.num_rows(), 1);
        assert_eq!(
            kept.column_by_name(spans::TRACE_ID)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::FixedBytes16(*span_row(3, 0, 0, 0).trace_id.as_bytes())
        );

        // Also check with non-All Selection and Const column encoding.
        let mut cols = col_batch.columns().to_vec();
        let ingest_idx = col_batch.schema().index_of(spans::INGEST_TIME).unwrap();
        cols[ingest_idx] = Arc::new(
            Column::try_new_const(
                ScalarValue::TimestampNs {
                    value: 1,
                    utc: true,
                },
                3,
            )
            .unwrap(),
        );
        let with_const = Batch::try_new_shared(
            Arc::clone(col_batch.schema()),
            cols,
            3,
            Selection::Indices(vec![2, 0]),
        )
        .unwrap();
        let kept_const = drop_covered_batch(&with_const, &pred).unwrap();
        assert_eq!(kept_const.num_rows(), 1);
        assert_eq!(
            kept_const
                .column_by_name(spans::TRACE_ID)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::FixedBytes16(*span_row(3, 0, 0, 0).trace_id.as_bytes())
        );
    }
}
