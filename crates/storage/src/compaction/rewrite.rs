//! The deletion pass: rewrite the files that hold covered rows, raise the
//! clean ones, then mark the deletions applied (spec 03 §7).
//!
//! The pass runs after each compaction pass and is also exposed as
//! [`crate::Storage::apply_deletions`] for tests. Compaction and the rewriter
//! never run on the same file: both claim their files first.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use cetra_column::{CancelToken, DataType, ScalarValue, Schema};
use cetra_core::ids::ProjectId;
use cetra_core::schema::{scores, spans};
use cetra_query::exec::{
    ExecContext, ExecMetrics, IdLookupHint, ScanSource, ScanSpec, SpillManager, TimeRangeHint,
};
use cetra_query::tombstone::{TombstoneTable, covered_predicate};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError};
use chrono::NaiveDate;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::catalog::{CatalogEdit, FileId, FileMeta, FileTable, now_ns};
use crate::compaction::job::{ClaimGuard, Compactor};
use crate::compaction::merge::MergeContext;
use crate::compaction::plan::{Job, JobKind, Policy};
use crate::deletion::{PendingDeletion, Tombstones, applicable, to_tombstone_specs};
use crate::engine::statam_source::StatamSource;
use crate::error::{CommitError, CompactionError};
use crate::memtable::MemSnapshot;
use crate::read_state::{ReadState, publish_catalog, publish_tombstones};
use crate::statam_io::ingest_column;
use crate::verify::{Suspects, may_be_corrupt};

/// What one deletion pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionReport {
    /// Files rewritten without their covered rows.
    pub rewritten: u64,
    /// Files checked and found clean, whose `deletions_applied_seq` was raised.
    pub raised: u64,
    /// Deletions marked applied.
    pub applied: u64,
    /// Rewrites or raise commits that failed or lost a conflict; a later pass
    /// redoes them.
    pub failed: u64,
    /// Projects with a deletion applied in this pass, in order, without repeats.
    pub applied_projects: Vec<ProjectId>,
}

/// What one candidate came to.
enum Checked {
    /// The file was rewritten without its covered rows.
    Rewritten,
    /// The file holds no covered row: its id and the `deletions_applied_seq`
    /// the check saw, for the raise.
    Clean { id: FileId, applied: u64 },
    /// The rewrite failed or lost a conflict; logged where it ran.
    Failed,
}

impl Compactor {
    /// One deletion pass at `now` (spec 03 §7): check the candidate files,
    /// rewrite the covered ones, raise the clean ones, then mark every
    /// deletion applied whose rows are gone.
    ///
    /// The pass itself never fails: a candidate or commit that fails is
    /// counted in [`DeletionReport::failed`] and a later pass redoes it, so
    /// one bad file never blocks the other deletions.
    pub(crate) async fn deletion_pass(
        self: &Arc<Self>,
        now: i64,
    ) -> Result<DeletionReport, CompactionError> {
        // A stopped store (D-R-6) would only stage rows that cannot commit.
        if self.objects.sync_failed() || self.committer.stopped() {
            tracing::debug!("deletion pass skipped: the store stopped after an I/O error");
            return Ok(DeletionReport::default());
        }
        // 1. The tombstones as last published.
        let tombstones = Arc::clone(&self.published.load().tombstones);
        let pending = tombstones.all();
        if pending.is_empty() {
            return Ok(DeletionReport::default());
        }
        let known = tombstones.known_seq();
        let policy = *self.policy.lock().unwrap_or_else(PoisonError::into_inner);
        let mut report = DeletionReport::default();

        // 2. One warning per deletion past `storage.deletion.max_delay`.
        {
            let mut overdue = self
                .overdue_logged
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for deletion in &pending {
                if deletion.registered_at.saturating_add(self.max_delay) < now
                    && overdue.insert(deletion.id)
                {
                    // Never log the scope: its ids name the person the
                    // deletion is about (spec 03 §7).
                    tracing::warn!(
                        deletion_id = %deletion.id,
                        project = %deletion.project,
                        seq = deletion.seq,
                        "deletion is past storage.deletion.max_delay"
                    );
                }
            }
        }

        // 3. Candidates: the active `spans` and `scores` files a pending
        // deletion may cover, per project in id order.
        let catalog = self.committer.catalog().await;
        let mut projects: Vec<ProjectId> =
            pending.iter().map(|deletion| deletion.project).collect();
        projects.sort();
        projects.dedup();
        let mut candidates: Vec<Arc<FileMeta>> = Vec::new();
        for project in projects {
            let deletions = tombstones.for_project(project);
            for table in [FileTable::Spans, FileTable::Scores] {
                for file in catalog.files(table, project, NaiveDate::MIN..=NaiveDate::MAX) {
                    let covering = applicable(
                        &deletions,
                        Some(file.deletions_applied_seq),
                        file.min_ingest_time,
                    );
                    if !covering.is_empty() {
                        candidates.push(Arc::clone(file));
                    }
                }
            }
            // A quarantined file a deletion may cover cannot be rewritten: it
            // is deleted instead (spec 03 §10). GC collects it once it is
            // obsolete.
            let quarantined = match self.committer.quarantined(project).await {
                Ok(quarantined) => quarantined,
                Err(_) => {
                    report.failed += 1;
                    tracing::warn!(project = %project, "listing quarantined files failed");
                    Vec::new()
                }
            };
            let doomed: Vec<FileId> = quarantined
                .into_iter()
                .filter(|(_, _, min_ingest_time, applied)| {
                    !applicable(&deletions, Some(*applied), *min_ingest_time).is_empty()
                })
                .map(|(id, _, _, _)| id)
                .collect();
            if !doomed.is_empty() {
                match self.committer.retire_quarantined(&doomed).await {
                    Ok(()) => tracing::warn!(
                        project = %project,
                        files = doomed.len(),
                        "quarantined files deleted for a pending deletion"
                    ),
                    Err(_) => {
                        report.failed += 1;
                        tracing::warn!(project = %project, "retiring quarantined files failed");
                    }
                }
            }
        }

        // 4. Claim: a file a running job holds is left for a later pass.
        let mut claimed: Vec<(Arc<FileMeta>, ClaimGuard)> = Vec::new();
        {
            let mut claims = self.claims.lock().unwrap_or_else(PoisonError::into_inner);
            for file in candidates {
                if claims.contains(&file.id) {
                    continue;
                }
                claims.insert(file.id);
                claimed.push((
                    Arc::clone(&file),
                    ClaimGuard {
                        compactor: Arc::clone(self),
                        ids: vec![file.id],
                    },
                ));
            }
        }

        // 5. Check and rewrite, at most `threads` at a time. Each guard moves
        // into its task; the claim is released when the task ends. A file
        // whose check fails is counted and left to a later pass, so it cannot
        // abort the other candidates or the steps below.
        let semaphore = Arc::new(Semaphore::new(self.threads.max(1)));
        let mut set: JoinSet<(FileId, Result<Checked, CompactionError>)> = JoinSet::new();
        for (file, guard) in claimed {
            let permit = match Arc::clone(&semaphore).acquire_owned().await {
                Ok(permit) => permit,
                // The semaphore is never closed.
                Err(_) => unreachable!("the compaction semaphore is never closed"),
            };
            let compactor = Arc::clone(self);
            let tombstones = Arc::clone(&tombstones);
            set.spawn(async move {
                let _permit = permit;
                let _guard = guard;
                (
                    file.id,
                    check_and_rewrite(&compactor, &tombstones, &file, policy).await,
                )
            });
        }
        let mut clean: Vec<(FileId, u64)> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, Ok(Checked::Rewritten))) => report.rewritten += 1,
                Ok((_, Ok(Checked::Clean { id, applied }))) => clean.push((id, applied)),
                Ok((_, Ok(Checked::Failed))) => report.failed += 1,
                Ok((id, Err(error))) => {
                    // The label only, so covered-row predicates never leak
                    // literals into logs (spec 03 §7).
                    report.failed += 1;
                    tracing::warn!(
                        file = %id,
                        error = error.label(),
                        "checking a file for deleted rows failed"
                    );
                }
                Err(_) => {
                    report.failed += 1;
                    tracing::warn!("deletion task failed");
                }
            }
        }

        // 6. Raise every clean file to the known sequence in one edit.
        let raise: Vec<(FileId, u64)> = clean
            .into_iter()
            .filter(|(_, applied)| *applied < known)
            .map(|(id, _)| (id, known))
            .collect();
        if !raise.is_empty() {
            let raised = u64::try_from(raise.len()).unwrap_or(u64::MAX);
            let edit = CatalogEdit {
                raise,
                ..CatalogEdit::default()
            };
            let published = Arc::clone(&self.published);
            match self
                .committer
                .commit(&edit, move |catalog| {
                    publish_catalog(&published, catalog);
                })
                .await
            {
                Ok(_) => report.raised += raised,
                Err(error) => {
                    // A conflict means the file was compacted or rewritten
                    // meanwhile; the next pass checks it again.
                    report.failed += 1;
                    if matches!(error, CommitError::Conflict { .. }) {
                        tracing::info!(
                            %error,
                            "raising deletions_applied_seq lost a conflict; a later pass redoes it"
                        );
                    } else {
                        tracing::warn!(%error, "raising deletions_applied_seq failed");
                    }
                }
            }
        }

        // 7. Every claim is released: the guards moved into the tasks, and the
        // tasks have all ended.

        // 8. Applied. One published snapshot holds the catalog and both
        // memtables: a flush that commits between two reads could otherwise
        // drop its batch from the newer state while the older catalog still
        // lacks the file, and the deletion would be marked applied with its
        // rows still in that file (spec 03 §7).
        let state = self.published.load();
        for deletion in &pending {
            if !applied_in_state(&state, deletion) {
                continue;
            }
            let published = Arc::clone(&self.published);
            match self
                .committer
                .mark_applied(deletion.project, deletion.id, now, move |tombstones| {
                    publish_tombstones(&published, tombstones);
                })
                .await
            {
                Ok(true) => {
                    report.applied += 1;
                    if !report.applied_projects.contains(&deletion.project) {
                        report.applied_projects.push(deletion.project);
                    }
                    tracing::info!(
                        deletion_id = %deletion.id,
                        project = %deletion.project,
                        seq = deletion.seq,
                        "deletion applied"
                    );
                    self.overdue_logged
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&deletion.id);
                }
                Ok(false) => {}
                Err(_) => {
                    // The catalog error text holds no scope value, but keep
                    // the line short.
                    report.failed += 1;
                    tracing::warn!(
                        deletion_id = %deletion.id,
                        "marking a deletion applied failed"
                    );
                }
            }
        }

        // 9. The report.
        Ok(report)
    }
}

/// Step 5 for one claimed candidate: check it for a covered row, then rewrite
/// it or hand it back for the raise.
async fn check_and_rewrite(
    compactor: &Arc<Compactor>,
    tombstones: &Arc<Tombstones>,
    file: &Arc<FileMeta>,
    policy: Policy,
) -> Result<Checked, CompactionError> {
    let col_schema = table_column_schema(file.table)?;
    let tombstone_table = tombstone_table_of(file.table)?;
    let deletions = applicable(
        &tombstones.for_project(file.project),
        Some(file.deletions_applied_seq),
        file.min_ingest_time,
    );
    let specs = to_tombstone_specs(&deletions);
    // Not `None`: the list is not empty and every pending deletion has at
    // least one term for `spans` and `scores`. Treated as clean regardless.
    let Some(covered) = covered_predicate(tombstone_table, &col_schema, &specs).map_err(|_| {
        CompactionError::Deletion {
            reason: "building the deletion filter",
        }
    })?
    else {
        return Ok(Checked::Clean {
            id: file.id,
            applied: file.deletions_applied_seq,
        });
    };
    let holds = match holds_covered(&compactor.merge, file, &col_schema, &covered, &deletions).await
    {
        Ok(holds) => holds,
        Err(error) => {
            if let CompactionError::Read { source } = &error
                && may_be_corrupt(source)
            {
                compactor.verify.suspect([file.id], now_ns());
            }
            return Err(error);
        }
    };
    if !holds {
        return Ok(Checked::Clean {
            id: file.id,
            applied: file.deletions_applied_seq,
        });
    }
    let job = Job {
        table: file.table,
        project: file.project,
        day: file.day,
        kind: JobKind::Rewrite,
        inputs: vec![Arc::clone(file)],
        whole_partition: false,
        drop_deleted: false,
    };
    let table = file.table.as_str();
    let project = file.project.to_string();
    let day = file.day.to_string();
    match compactor.run(job, policy).await {
        Ok(_) => Ok(Checked::Rewritten),
        Err(error) => {
            // Logged like a failed compaction job: the label only, since the
            // error may quote the covered-row predicate's literals.
            if matches!(error, CompactionError::Commit(CommitError::Conflict { .. })) {
                tracing::info!(
                    table,
                    project,
                    day,
                    kind = "rewrite",
                    inputs = 1,
                    %error,
                    "compaction job lost a conflict; a later pass redoes it"
                );
            } else if matches!(error, CompactionError::DiskFull) {
                tracing::info!(
                    table,
                    project,
                    day,
                    kind = "rewrite",
                    inputs = 1,
                    error = error.label(),
                    "compaction job deferred: not enough free disk"
                );
            } else {
                tracing::warn!(
                    table,
                    project,
                    day,
                    kind = "rewrite",
                    inputs = 1,
                    error = error.label(),
                    source = error.read_source(),
                    "compaction job failed"
                );
            }
            Ok(Checked::Failed)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SingleTarget {
    Trace,
    User,
    Session,
}

/// Populates `ScanSpec::id_lookups` ONLY when every active deletion on `table`
/// targets the exact same single column, unioning all values into one hint.
fn single_target_id_lookup(
    table: FileTable,
    deletions: &[Arc<PendingDeletion>],
) -> Vec<IdLookupHint> {
    let mut common_target: Option<SingleTarget> = None;
    let mut trace_ids: BTreeSet<[u8; 16]> = BTreeSet::new();
    let mut users: BTreeSet<String> = BTreeSet::new();
    let mut sessions: BTreeSet<String> = BTreeSet::new();

    for deletion in deletions {
        let has_trace = !deletion.trace_ids.is_empty();
        let has_user = table != FileTable::Traces && !deletion.users.is_empty();
        let has_session = table == FileTable::Scores && !deletion.sessions.is_empty();
        let active_cols = usize::from(has_trace) + usize::from(has_user) + usize::from(has_session);
        if active_cols == 0 {
            continue;
        }
        if active_cols > 1 {
            return Vec::new();
        }
        let target = if has_trace {
            SingleTarget::Trace
        } else if has_user {
            SingleTarget::User
        } else {
            SingleTarget::Session
        };
        match common_target {
            None => common_target = Some(target),
            Some(existing) if existing != target => return Vec::new(),
            Some(_) => {}
        }
        match target {
            SingleTarget::Trace => trace_ids.extend(deletion.trace_ids.iter().copied()),
            SingleTarget::User => users.extend(deletion.users.iter().cloned()),
            SingleTarget::Session => sessions.extend(deletion.sessions.iter().cloned()),
        }
    }

    match common_target {
        Some(SingleTarget::Trace) if !trace_ids.is_empty() => vec![IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: trace_ids.into_iter().collect(),
        }],
        Some(SingleTarget::User) if !users.is_empty() => vec![IdLookupHint::Utf8 {
            column: spans::USER_ID.to_owned(),
            values: users.into_iter().collect(),
        }],
        Some(SingleTarget::Session) if !sessions.is_empty() => vec![IdLookupHint::Utf8 {
            column: scores::SESSION_ID.to_owned(),
            values: sessions.into_iter().collect(),
        }],
        _ => Vec::new(),
    }
}

/// Does `file` hold a row covered by `deletions`? A `LIMIT 1` scan of the key
/// columns with the covered-row predicate pushed down (spec 03 §7):
/// statistics prune row groups on `ingest_time`, the bloom filters on
/// `trace_id` (plus `user_id` and `session_id` when single-target).
async fn holds_covered(
    ctx: &MergeContext,
    file: &Arc<FileMeta>,
    col_schema: &Arc<Schema>,
    covered: &Expr,
    deletions: &[Arc<PendingDeletion>],
) -> Result<bool, CompactionError> {
    let tombstone_table = tombstone_table_of(file.table)?;
    let keys = cetra_query::tombstone::key_columns(tombstone_table);
    let mut key_indices = Vec::with_capacity(keys.len());
    for name in keys {
        key_indices.push(col_schema.index_of(name).ok_or(CompactionError::Deletion {
            reason: "building the deletion filter",
        })?);
    }

    let Some(max_cutoff) = deletions.iter().map(|d| d.cutoff).max() else {
        return Ok(false);
    };
    let ingest_col = ingest_column(file.table);
    let ingest_idx = col_schema
        .index_of(ingest_col)
        .ok_or(CompactionError::Deletion {
            reason: "building the deletion filter",
        })?;
    let DataType::TimestampNs { utc } = col_schema.fields()[ingest_idx].data_type else {
        return Err(CompactionError::Deletion {
            reason: "building the deletion filter",
        });
    };
    let ingest_lte_cutoff = Expr::Binary {
        left: Box::new(Expr::Column {
            index: ingest_idx,
            name: ingest_col.to_owned(),
            data_type: DataType::TimestampNs { utc },
        }),
        op: BinaryOp::LtEq,
        right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
            value: max_cutoff,
            utc,
        })),
        data_type: DataType::Bool,
    };

    let spec = ScanSpec {
        projection: Some(key_indices),
        // Leave `time_range` open: `time_range` prunes `start_time`/`created_at`,
        // NOT `ingest_time`. `ingest_lte_cutoff` prunes `ingest_time` instead.
        time_range: TimeRangeHint::default(),
        id_lookups: single_target_id_lookup(file.table, deletions),
        filters: vec![covered.clone(), ingest_lte_cutoff],
        limit: Some(1),
        ..ScanSpec::default()
    };

    let scoped_pool = ctx
        .pool
        .try_admit(0, ctx.pool.limit())
        .ok_or_else(|| CompactionError::from(QueryError::TooLarge))?;
    let workers = ctx.qe_runtime.bg_workers().map_err(CompactionError::from)?;
    let cancel = CancelToken::new();
    let _cancel_guard = super::CancelOnDrop(cancel.clone());
    let exec_ctx = ExecContext {
        pool: Arc::clone(&scoped_pool),
        eval: EvalContext::new(now_ns(), cancel.clone()),
        spill: Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0)),
        workers: Arc::clone(&workers),
        batch_size: ctx.batch_size,
        target_partitions: 1,
        metrics: ExecMetrics::new(),
    };

    // Inline tombstones MUST be disabled (`Arc::from([])`) so covered rows
    // are not stripped before `spec.filters` sees them.
    let mut streams = StatamSource::new(
        ctx.io.clone(),
        file.table,
        Arc::clone(col_schema),
        vec![Arc::clone(file)],
        Arc::from([]),
        Arc::clone(&ctx.qe_runtime.metadata_cache),
        Suspects::new(Arc::clone(&ctx.verify)),
        ctx.runtime_handle.clone(),
    )
    .open_partitions(&spec, &exec_ctx)
    .map_err(CompactionError::from)?;
    let Some(mut stream) = streams.pop() else {
        return Ok(false);
    };

    tokio::task::spawn_blocking(move || {
        let task_cancel = cancel.clone();
        let mut out = workers
            .run_scoped(
                &cancel,
                vec![move |_worker_idx| {
                    let mut res = scoped_pool.register("holds_covered", false);
                    loop {
                        res.free();
                        match stream.next_batch(&task_cancel, Some(&mut res))? {
                            Some(batch) => {
                                if batch.num_rows() > 0 {
                                    res.free();
                                    return Ok(true);
                                }
                            }
                            None => {
                                res.free();
                                return Ok(false);
                            }
                        }
                    }
                }],
            )
            .map_err(CompactionError::from)?;
        out.pop().ok_or(CompactionError::Task)
    })
    .await
    .map_err(|_| CompactionError::Task)?
}

fn tombstone_table_of(table: FileTable) -> Result<TombstoneTable, CompactionError> {
    match table {
        FileTable::Spans => Ok(TombstoneTable::Spans),
        FileTable::Scores => Ok(TombstoneTable::Scores),
        FileTable::Traces => Err(CompactionError::Input {
            reason: "traces are not rewritten",
        }),
    }
}

/// The stored columnar schema of a rewritten table.
fn table_column_schema(table: FileTable) -> Result<Arc<Schema>, CompactionError> {
    match table {
        FileTable::Spans | FileTable::Scores => Ok(table.column_schema()),
        FileTable::Traces => Err(CompactionError::Input {
            reason: "traces are not rewritten",
        }),
    }
}

/// Step 8's test for one deletion, against one published snapshot: every
/// `spans` and `scores` file of its project that could hold a covered row is
/// clean of it, and no memtable batch could hold one (spec 03 §7).
fn applied_in_state(state: &ReadState, deletion: &PendingDeletion) -> bool {
    for table in [FileTable::Spans, FileTable::Scores] {
        let clean = state
            .catalog
            .files(table, deletion.project, NaiveDate::MIN..=NaiveDate::MAX)
            .filter(|file| file.min_ingest_time <= deletion.cutoff)
            .all(|file| file.deletions_applied_seq >= deletion.seq);
        if !clean {
            return false;
        }
    }
    !batch_in_scope(&state.spans, deletion) && !batch_in_scope(&state.scores, deletion)
}

/// Does the snapshot hold a batch of the project with `min_ingest_time` at or
/// before the cut-off? A batch that held covered rows is dropped by the next
/// flush, which writes none of them (spec 03 §7).
fn batch_in_scope(snapshot: &MemSnapshot, deletion: &PendingDeletion) -> bool {
    snapshot
        .partitions
        .iter()
        .filter(|(key, _)| key.project == deletion.project)
        .flat_map(|(_, batches)| batches)
        .any(|batch| batch.min_ingest_time <= deletion.cutoff)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, PoisonError};
    use std::time::{Duration, Instant};

    use cetra_core::ids::ProjectId;
    use cetra_core::schema::traces;
    use cetra_core::score::ScoreTarget;
    use uuid::Uuid;

    use crate::QueryOptions;
    use crate::Storage;
    use crate::catalog::{FileMeta, FileTable, now_ns};
    use crate::compaction::job::tests::{
        at, config_in, flush_cycle, flush_scores, project, quiet_compaction, score_file_rows,
        wait_at,
    };
    use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
    use crate::hooks::HookPoint;
    use crate::reads::columns::{active_rows, i64_at, require_col};
    use crate::reads::{ListScope, MAX_PAGE, SortOrder, TraceListRequest, TraceSort, trace_list};
    use crate::test_support::{object_rows, rows, score_row, score_rows, span_row, span_rows};
    use crate::views::QueryContext;
    use crate::wal::Table;

    /// `span_count` of every trace `trace_list` returns for `ctx`, in list order.
    async fn span_counts(ctx: &QueryContext) -> Vec<i64> {
        let page = trace_list(
            ctx,
            TraceListRequest {
                filter: None,
                search: None,
                scope: ListScope {
                    window: None,
                    include_eval: true,
                    include_experiments: true,
                },
                strict_exclusions: false,
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: None,
                timeout: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
        let mut counts = Vec::new();
        for batch in &page.batches {
            let col = require_col(batch, traces::SPAN_COUNT).unwrap();
            for row in 0..batch.num_rows() {
                counts.push(i64_at(batch, col, row).unwrap());
            }
        }
        counts
    }

    /// A query context on the fixed test project's current snapshot.
    async fn snapshot(storage: &Storage) -> QueryContext {
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        QueryContext::new(query, None).unwrap()
    }

    /// One `traces` deletion of the given traces, cut off at `cutoff`.
    fn traces_deletion(traces: &[u16], cutoff: i64) -> Deletion {
        traces_deletion_in(project(), traces, cutoff)
    }

    /// [`traces_deletion`] for another project than the fixed test one.
    fn traces_deletion_in(project: ProjectId, traces: &[u16], cutoff: i64) -> Deletion {
        Deletion {
            id: Uuid::now_v7(),
            project,
            cutoff,
            scope: DeletionScope::Traces {
                trace_ids: traces
                    .iter()
                    .map(|trace| span_row(*trace, 0, 0, 0).trace_id)
                    .collect(),
            },
        }
    }

    /// The second fixed test project, for two-project tests.
    fn other_project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap()
    }

    fn trace_bytes(trace: u16) -> [u8; 16] {
        *span_row(trace, 0, 0, 0).trace_id.as_bytes()
    }

    /// The raw 16-byte trace ids stored in one Statam object.
    fn object_trace_ids(dir: &Path, file: &FileMeta, column: &str) -> Vec<[u8; 16]> {
        let all = object_rows(dir, file);
        let col = all.column_by_name(column).unwrap();
        match col {
            cetra_column::Column::Flat(flat) => match flat.data() {
                cetra_column::FlatData::FixedBytes16(arr) => {
                    active_rows(&all).map(|row| arr[row]).collect()
                }
                other => panic!("unexpected trace_id flat data: {other:?}"),
            },
            other => panic!("unexpected trace_id encoding: {other:?}"),
        }
    }

    fn spans_trace_ids(dir: &Path, file: &FileMeta) -> Vec<[u8; 16]> {
        object_trace_ids(dir, file, cetra_core::schema::spans::TRACE_ID)
    }

    fn traces_trace_ids(dir: &Path, file: &FileMeta) -> Vec<[u8; 16]> {
        object_trace_ids(dir, file, traces::TRACE_ID)
    }

    /// The first integer scalar of the first row of `sql`.
    async fn scalar(storage: &Storage, sql: &str) -> i64 {
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let ctx = QueryContext::new(query, None).unwrap();
        let result = rows(&ctx, sql).await;
        result[0].values().next().unwrap().as_i64().unwrap()
    }

    async fn span_count(storage: &Storage, trace: u16) -> i64 {
        let hex = span_row(trace, 0, 0, 0).trace_id.to_hex();
        scalar(
            storage,
            &format!("SELECT count(*) AS n FROM spans WHERE trace_id = '{hex}'"),
        )
        .await
    }

    /// The deletion's status, unwrapped.
    async fn status(storage: &Storage, deletion: &Deletion) -> DeletionStatus {
        storage.deletion_status(deletion.id).await.unwrap()
    }

    /// The ids the pass has warned about as overdue, copied out.
    fn overdue_logged(storage: &Storage) -> std::collections::HashSet<Uuid> {
        storage
            .compactor
            .overdue_logged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    // 1. Compaction drops covered rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compaction_drops_covered_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(3), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        for index in 0..3_u8 {
            let base = at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
            storage = flush_cycle(
                &config,
                storage,
                span_rows(&[
                    span_row(1, 10 + index, base, base + 1_000_000_000),
                    span_row(2, 20 + index, base, base + 2_000_000_000),
                ]),
            )
            .await;
        }
        assert_eq!(storage.catalog_files(Table::Spans).len(), 3);
        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();

        let report = storage.compact(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].level, 1);
        assert_eq!(files[0].deletions_applied_seq, 1);
        let stored = spans_trace_ids(dir.path(), &files[0]);
        assert_eq!(stored, vec![trace_bytes(2); 3], "trace 2 only, three spans");
        let traces_files = storage.catalog_files(FileTable::Traces);
        assert_eq!(traces_files.len(), 1);
        assert_eq!(traces_files[0].deletions_applied_seq, 1);

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (0, 1, 0),
            "{report:?}"
        );
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        storage.shutdown().await.unwrap();
    }

    // 0. A context reads one snapshot: a compaction pass between two reads on it leaves both
    // reads unchanged, GC keeps the input files while the context lives, and a fresh context
    // sees the merged file (one copy of the span).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_context_keeps_its_snapshot_across_compaction_and_gc() {
        let dir = tempfile::tempdir().unwrap();
        // The compaction interval is an hour, so its first tick cannot fire during this test.
        let config = config_in(dir.path(), quiet_compaction(2), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let base = at(10, 0, 0);
        for _ in 0..2 {
            storage = flush_cycle(
                &config,
                storage,
                span_rows(&[span_row(1, 0, base, base + 1)]),
            )
            .await;
        }
        assert_eq!(storage.catalog_files(Table::Spans).len(), 2);
        let inputs: Vec<String> = storage
            .catalog_files(Table::Spans)
            .iter()
            .chain(storage.catalog_files(FileTable::Traces).iter())
            .map(|file| file.path.clone())
            .collect();
        let ctx = snapshot(&storage).await;
        assert_eq!(span_counts(&ctx).await, vec![2]);

        let report = storage.compact(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
        assert_eq!(storage.catalog_files(Table::Spans).len(), 1);

        // Zero grace: the inputs are collectable at once, but the live context still names them.
        let hooks = crate::hooks::Hooks::default();
        let held = crate::gc::collect_obsolete(
            &storage.committer,
            &storage.objects,
            &hooks,
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(held.deleted, 0, "{held:?}");
        assert!(held.in_use >= 1, "{held:?}");
        for path in &inputs {
            assert!(dir.path().join("objects").join(path).exists(), "{path}");
        }
        assert_eq!(
            span_counts(&ctx).await,
            vec![2],
            "the context keeps its snapshot"
        );

        drop(ctx);
        let freed = crate::gc::collect_obsolete(
            &storage.committer,
            &storage.objects,
            &hooks,
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(freed.deleted, inputs.len() as u64, "{freed:?}");
        for path in &inputs {
            assert!(!dir.path().join("objects").join(path).exists(), "{path}");
        }

        let fresh = snapshot(&storage).await;
        assert_eq!(span_counts(&fresh).await, vec![1]);
        storage.shutdown().await.unwrap();
    }

    // 2. Rewrite of an L0 and an L1 file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rewrite_keeps_each_inputs_level_and_fully_merged() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(2), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let times = [at(10, 0, 0), at(10, 1, 0)];
        for (index, time) in times.iter().enumerate() {
            storage = flush_cycle(
                &config,
                storage,
                span_rows(&[
                    span_row(1, index as u8, *time, time + 1),
                    span_row(2, index as u8, *time, time + 1),
                ]),
            )
            .await;
        }
        let report = storage.compact(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
        let l1 = storage.catalog_files(Table::Spans);
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0].level, 1);
        assert!(l1[0].fully_merged);

        // A late flush makes an L0 file of the same partition: both levels
        // now hold trace 1.
        let late = at(10, 2, 0);
        storage = flush_cycle(
            &config,
            storage,
            span_rows(&[
                span_row(1, 2, late, late + 1),
                span_row(2, 2, late, late + 1),
            ]),
        )
        .await;
        assert_eq!(storage.catalog_files(Table::Spans).len(), 2);

        let deletion = traces_deletion(&[1], at(12, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.apply_deletions(at(12, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (2, 1, 0),
            "{report:?}"
        );

        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 2);
        let mut levels: Vec<(u8, bool)> = Vec::new();
        for file in &files {
            assert!(!spans_trace_ids(dir.path(), file).contains(&trace_bytes(1)));
            assert_eq!(file.deletions_applied_seq, 1);
            levels.push((file.level, file.fully_merged));
        }
        assert!(levels.contains(&(0, false)), "the L0 rewrite: {levels:?}");
        assert!(levels.contains(&(1, true)), "the L1 rewrite: {levels:?}");

        // The reopen checks the `file_sources` pairs of the new files.
        storage.shutdown().await.unwrap();
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.catalog_files(Table::Spans).len(), 2);
        assert_eq!(storage.catalog_files(FileTable::Traces).len(), 2);
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(12, 0, 0)
            }
        );
        assert!(
            storage
                .published
                .load()
                .tombstones
                .for_project(project())
                .is_empty(),
            "the published tombstones are empty"
        );
        storage.shutdown().await.unwrap();
    }

    // 3. A clean candidate is raised, not rewritten.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_clean_candidate_is_raised_without_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        storage = flush_cycle(
            &config,
            storage,
            span_rows(&[span_row(2, 0, time, time + 1)]),
        )
        .await;
        let id = storage.catalog_files(Table::Spans)[0].id;

        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (
                report.rewritten,
                report.raised,
                report.applied,
                report.failed
            ),
            (0, 1, 1, 0),
            "{report:?}"
        );
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, id, "the file is unchanged");
        assert_eq!(files[0].deletions_applied_seq, 1);
        storage.shutdown().await.unwrap();
    }

    // 4. Rows only in the memtable wait for the flush.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_memtable_only_deletion_waits_for_the_flush() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        storage
            .append_spans(project(), span_rows(&[span_row(1, 0, time, time + 1)]))
            .await
            .unwrap();
        let deletion = traces_deletion(&[1], time + 1);
        storage.register_deletion(deletion.clone()).await.unwrap();

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.applied, report.failed), (0, 0), "{report:?}");
        assert_eq!(status(&storage, &deletion).await, DeletionStatus::Pending);

        // The flush drops the covered rows and writes no file (M5.3).
        storage.shutdown().await.unwrap();
        let storage = Storage::open(config).await.unwrap();
        assert!(storage.catalog_files(Table::Spans).is_empty());
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.applied, report.failed), (1, 0), "{report:?}");
        storage.shutdown().await.unwrap();
    }

    // 5. A file whose rows are all covered disappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fully_covered_file_is_removed_by_the_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        storage = flush_cycle(
            &config,
            storage,
            span_rows(&[
                span_row(1, 0, time, time + 1),
                span_row(1, 1, time, time + 1),
            ]),
        )
        .await;
        assert_eq!(storage.catalog_files(Table::Spans).len(), 1);

        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (1, 1, 0),
            "{report:?}"
        );
        assert!(storage.catalog_files(Table::Spans).is_empty());
        assert!(storage.catalog_files(FileTable::Traces).is_empty());
        assert_eq!(
            scalar(&storage, "SELECT count(*) FROM spans").await,
            0,
            "the view is empty"
        );
        storage.shutdown().await.unwrap();
    }

    // 5b. A rewrite that does not fit the free disk is deferred: the deletion
    //     stays pending, and a later pass rewrites the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rewrite_that_does_not_fit_is_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        storage = flush_cycle(
            &config,
            storage,
            span_rows(&[
                span_row(1, 0, time, time + 1),
                span_row(2, 0, time, time + 1),
            ]),
        )
        .await;
        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();
        let before = storage.catalog_paths(FileTable::Spans);

        storage.disk().set_free_for_test(Some(0));
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (0, 0, 1),
            "{report:?}"
        );
        assert_eq!(status(&storage, &deletion).await, DeletionStatus::Pending);
        assert_eq!(storage.catalog_paths(FileTable::Spans), before);

        storage.disk().set_free_for_test(None);
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (1, 1, 0),
            "{report:?}"
        );
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        storage.shutdown().await.unwrap();
    }

    // 6. The milestone race: a compaction started before a deletion commits
    //    after it, and the rows stay gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_compaction_started_before_a_deletion_is_rewritten_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(2), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        for index in 0..2_u8 {
            let base = at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
            storage = flush_cycle(
                &config,
                storage,
                span_rows(&[
                    span_row(1, 10 + index, base, base + 1_000_000_000),
                    span_row(2, 20 + index, base, base + 2_000_000_000),
                ]),
            )
            .await;
        }
        let storage = Arc::new(storage);
        let hooks = storage.hooks().clone();
        hooks.hold(HookPoint::CompactionAfterWrite);
        let racing = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.compact(at(11, 0, 0)).await })
        };
        wait_at(&storage, HookPoint::CompactionAfterWrite);

        // Registered while the job is parked: its outputs record the older
        // known sequence, so they stay candidates for the rewriter.
        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();
        hooks.release(HookPoint::CompactionAfterWrite);
        racing.await.unwrap().unwrap();

        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].deletions_applied_seq, 0);
        assert!(
            spans_trace_ids(dir.path(), &files[0]).contains(&trace_bytes(1)),
            "the L1 output still holds trace 1"
        );
        assert_eq!(span_count(&storage, 1).await, 0, "the query hides it");
        assert_eq!(span_count(&storage, 2).await, 2);

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (1, 1, 0),
            "{report:?}"
        );
        for file in storage.catalog_files(Table::Spans) {
            assert!(!spans_trace_ids(dir.path(), &file).contains(&trace_bytes(1)));
        }
        for file in storage.catalog_files(FileTable::Traces) {
            assert!(!traces_trace_ids(dir.path(), &file).contains(&trace_bytes(1)));
        }
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    // 7. A file claimed by a running job is left alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_deletion_pass_leaves_claimed_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(2), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        for index in 0..2_u8 {
            let base = at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
            storage = flush_cycle(
                &config,
                storage,
                span_rows(&[
                    span_row(1, 10 + index, base, base + 1_000_000_000),
                    span_row(2, 20 + index, base, base + 2_000_000_000),
                ]),
            )
            .await;
        }
        let deletion = traces_deletion(&[1], at(11, 0, 0));
        storage.register_deletion(deletion.clone()).await.unwrap();

        let storage = Arc::new(storage);
        let hooks = storage.hooks().clone();
        hooks.hold(HookPoint::CompactionAfterStage);
        let compacting = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.compact(at(11, 0, 0)).await })
        };
        wait_at(&storage, HookPoint::CompactionAfterStage);

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (
                report.rewritten,
                report.raised,
                report.applied,
                report.failed
            ),
            (0, 0, 0, 0),
            "{report:?}"
        );
        assert_eq!(storage.catalog_files(Table::Spans).len(), 2);
        assert_eq!(status(&storage, &deletion).await, DeletionStatus::Pending);

        hooks.release(HookPoint::CompactionAfterStage);
        compacting.await.unwrap().unwrap();
        // The merge applied the tombstones already; the next pass only marks
        // the deletion applied.
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (0, 1, 0),
            "{report:?}"
        );
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].deletions_applied_seq, 1);
        assert!(!spans_trace_ids(dir.path(), &files[0]).contains(&trace_bytes(1)));
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    // 8. A `user` deletion rewrites spans and scores, and keeps another
    //    user's span of the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_user_deletion_rewrites_spans_and_scores() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        let mut mine = span_row(1, 0, time, time + 1);
        mine.user_id = Some("u".to_owned());
        let mut shared = span_row(2, 0, time, time + 1);
        shared.user_id = Some("v".to_owned());
        shared.session_id = Some("s".to_owned());
        storage = flush_cycle(&config, storage, span_rows(&[mine, shared])).await;
        let mut session_score = score_row(0x11, 0, time, time, time + 1, false, "session");
        session_score.target = ScoreTarget::Session {
            session_id: "s".to_owned(),
        };
        let kept = score_row(0x22, 3, time, time, time + 1, false, "kept");
        storage = flush_scores(&config, storage, score_rows(&[session_score, kept])).await;

        let deletion = Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: time + 1,
            scope: DeletionScope::User {
                user_id: "u".to_owned(),
                trace_ids: Vec::new(),
                session_ids: vec!["s".to_owned()],
            },
        };
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (2, 1, 0),
            "{report:?}"
        );

        let spans_files = storage.catalog_files(Table::Spans);
        assert_eq!(spans_files.len(), 1);
        assert_eq!(
            spans_trace_ids(dir.path(), &spans_files[0]),
            vec![trace_bytes(2)],
            "the other user's span of the session stays"
        );
        let bytes = std::fs::read(dir.path().join("objects").join(&spans_files[0].path)).unwrap();
        let meta =
            cetra_statam::StatamFileMeta::read_from(&cetra_statam::SliceReader::new(bytes), None)
                .unwrap();
        assert_eq!(
            meta.postscript.format_version,
            cetra_statam::FORMAT_VERSION,
            "the rewritten file carries the Statam format version"
        );
        let stored = score_file_rows(dir.path(), &storage);
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert!(
            stored[0].0.ends_with("22"),
            "the score outside the scope stays: {stored:?}"
        );
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        storage.shutdown().await.unwrap();
    }

    // 9. An overdue deletion is warned about once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_overdue_deletion_is_warned_about_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        config.deletion.max_delay = Duration::from_secs(3_600);
        let storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        storage
            .append_spans(project(), span_rows(&[span_row(1, 0, time, time + 1)]))
            .await
            .unwrap();
        let deletion = traces_deletion(&[1], time + 1);
        storage.register_deletion(deletion.clone()).await.unwrap();

        let now = now_ns() + 2 * 3_600 * 1_000_000_000;
        let report = storage.apply_deletions(now).await.unwrap();
        assert_eq!(report.applied, 0, "the memtable batch still blocks it");
        let overdue = overdue_logged(&storage);
        assert_eq!(overdue.len(), 1);
        assert!(overdue.contains(&deletion.id));

        let report = storage.apply_deletions(now + 1).await.unwrap();
        assert_eq!(report.applied, 0);
        let overdue = overdue_logged(&storage);
        assert_eq!(overdue.len(), 1, "the second pass warns about nothing new");
        storage.shutdown().await.unwrap();
    }

    // 10. A flush prepared before the registration does not make the
    //     deletion look applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flush_in_flight_does_not_make_a_deletion_applied() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_millis(100));
        let storage = Arc::new(Storage::open(config.clone()).await.unwrap());
        let hooks = storage.hooks().clone();
        hooks.hold(HookPoint::FlushAfterWrite);
        let time = at(10, 0, 0);
        storage
            .append_spans(project(), span_rows(&[span_row(1, 0, time, time + 1)]))
            .await
            .unwrap();
        wait_at(&storage, HookPoint::FlushAfterWrite);

        // The flush was prepared before the registration: its file will keep
        // the row and record `deletions_applied_seq = 0`.
        let deletion = traces_deletion(&[1], time + 1);
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(report.applied, 0, "{report:?}");
        assert_eq!(status(&storage, &deletion).await, DeletionStatus::Pending);

        hooks.release(HookPoint::FlushAfterWrite);
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "the flush never committed"
            );
            if storage.catalog_files(Table::Spans).len() == 1
                && storage.published_rows(Table::Spans) == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files[0].deletions_applied_seq, 0);
        assert!(
            spans_trace_ids(dir.path(), &files[0]).contains(&trace_bytes(1)),
            "the flushed file still holds trace 1"
        );
        // Never applied while an active file still holds a covered row.
        assert_eq!(status(&storage, &deletion).await, DeletionStatus::Pending);

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (1, 1, 0),
            "{report:?}"
        );
        for file in storage.catalog_files(Table::Spans) {
            assert!(!spans_trace_ids(dir.path(), &file).contains(&trace_bytes(1)));
        }
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    // 11. A session-target score survives a covering merge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_session_score_survives_a_covering_merge() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(2), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        let mut session_score = score_row(0x11, 0, time, time, time, false, "session");
        session_score.target = ScoreTarget::Session {
            session_id: "s".to_owned(),
        };
        let on_trace = score_row(0x22, 1, time, time, time, false, "on-trace-1");
        storage = flush_scores(&config, storage, score_rows(&[session_score])).await;
        storage = flush_scores(&config, storage, score_rows(&[on_trace])).await;
        assert_eq!(storage.catalog_files(Table::Scores).len(), 2);

        let deletion = traces_deletion(&[1], time + 1);
        storage.register_deletion(deletion.clone()).await.unwrap();
        let report = storage.compact(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
        let files = storage.catalog_files(Table::Scores);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].deletions_applied_seq, 1);
        let stored = score_file_rows(dir.path(), &storage);
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert!(
            stored[0].0.ends_with("11"),
            "the session score survives the merge: {stored:?}"
        );
        storage.shutdown().await.unwrap();
    }

    // A read error on one candidate fails that file only; the other
    // projects' deletions still run.
    #[tokio::test]
    async fn a_corrupt_file_fails_only_its_own_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);
        let broken_row = span_row(1, 0, time, time + 1);
        let mut covered_row = span_row(2, 0, time, time + 1);
        covered_row.project_id = other_project();
        let mut kept_row = span_row(3, 0, time, time + 1);
        kept_row.project_id = other_project();
        storage
            .append_spans(project(), span_rows(&[broken_row]))
            .await
            .unwrap();
        storage
            .append_spans(other_project(), span_rows(&[covered_row, kept_row]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
        let storage = Storage::open(config).await.unwrap();
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 2);
        let broken = files
            .iter()
            .find(|file| file.project == project())
            .unwrap()
            .clone();
        let good_id = files
            .iter()
            .find(|file| file.project == other_project())
            .unwrap()
            .id;

        // Garbage bytes under the same key: the file's check will fail on read.
        std::fs::write(
            dir.path().join("objects").join(&broken.path),
            b"not a parquet object",
        )
        .unwrap();

        let broken_deletion = traces_deletion(&[1], time + 1);
        let good_deletion = traces_deletion_in(other_project(), &[2], time + 1);
        storage
            .register_deletion(broken_deletion.clone())
            .await
            .unwrap();
        storage
            .register_deletion(good_deletion.clone())
            .await
            .unwrap();

        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (1, 1, 1),
            "{report:?}"
        );
        assert_eq!(
            status(&storage, &good_deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        assert_eq!(
            status(&storage, &broken_deletion).await,
            DeletionStatus::Pending
        );

        // The healthy project's file was rewritten without its covered row;
        // the broken one is untouched.
        let good = storage
            .catalog_files(Table::Spans)
            .into_iter()
            .find(|file| file.project == other_project())
            .unwrap();
        assert_ne!(good.id, good_id, "the covered file was rewritten");
        // The known sequence spans projects: Q's deletion is the second one.
        assert_eq!(good.deletions_applied_seq, 2);
        assert_eq!(
            spans_trace_ids(dir.path(), &good),
            vec![trace_bytes(3)],
            "only the uncovered row remains"
        );
        let still_broken = storage
            .catalog_files(Table::Spans)
            .into_iter()
            .find(|file| file.id == broken.id)
            .unwrap();
        assert_eq!(still_broken.deletions_applied_seq, 0);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn holds_covered_disjunctive_user_deletion_never_has_false_negatives() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        let time = at(10, 0, 0);

        // File 1 (spans): matches ONLY `user_id = "erased-user"`, NOT `trace_ids`.
        let mut span_by_user = span_row(2, 0, time, time + 1);
        span_by_user.user_id = Some("erased-user".to_owned());
        let span_other = span_row(3, 0, time, time + 1);
        storage = flush_cycle(&config, storage, span_rows(&[span_by_user, span_other])).await;

        // File 2 (spans): matches ONLY `trace_id = trace 1`, NOT `user_id`.
        let span_by_trace = span_row(1, 0, time, time + 1);
        storage = flush_cycle(&config, storage, span_rows(&[span_by_trace])).await;

        // File 3 (scores): matches ONLY `session_id = "erased-sess"`, NOT `user_id` or `trace_id`.
        let mut score_by_session = score_row(0x11, 2, time, time, time + 1, false, "sess");
        score_by_session.target = ScoreTarget::Session {
            session_id: "erased-sess".to_owned(),
        };
        let score_kept = score_row(0x22, 3, time, time, time + 1, false, "kept");
        storage = flush_scores(
            &config,
            storage,
            score_rows(&[score_by_session, score_kept]),
        )
        .await;

        // File 4 (scores): matches ONLY `user_id = "erased-user"`, NOT `session_id` or `trace_id`.
        let mut score_by_user = score_row(0x33, 2, time, time, time + 1, false, "by-user");
        score_by_user.user_id = Some("erased-user".to_owned());
        storage = flush_scores(&config, storage, score_rows(&[score_by_user])).await;

        let deletion = Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: time + 1,
            scope: DeletionScope::User {
                user_id: "erased-user".to_owned(),
                trace_ids: vec![span_row(1, 0, 0, 0).trace_id],
                session_ids: vec!["erased-sess".to_owned()],
            },
        };
        storage.register_deletion(deletion.clone()).await.unwrap();

        // Direct `holds_covered` unit checks on each file + `single_target_id_lookup`.
        let tombstones = Arc::clone(&storage.published.load().tombstones);
        let project_deletions = tombstones.for_project(project());
        assert!(
            super::single_target_id_lookup(FileTable::Spans, &project_deletions).is_empty(),
            "disjunctive User deletion must NOT populate single-column id_lookups"
        );
        assert!(
            super::single_target_id_lookup(FileTable::Scores, &project_deletions).is_empty(),
            "disjunctive User deletion on scores must NOT populate id_lookups"
        );

        let spans_schema = FileTable::Spans.column_schema();
        let spans_covered = cetra_query::tombstone::covered_predicate(
            cetra_query::tombstone::TombstoneTable::Spans,
            &spans_schema,
            &crate::deletion::to_tombstone_specs(&project_deletions),
        )
        .unwrap()
        .unwrap();
        for file in storage.catalog_files(Table::Spans) {
            assert!(
                super::holds_covered(
                    &storage.compactor.merge,
                    &file,
                    &spans_schema,
                    &spans_covered,
                    &project_deletions,
                )
                .await
                .unwrap(),
                "spans file matching only one disjunct must be detected"
            );
        }

        let scores_schema = FileTable::Scores.column_schema();
        let scores_covered = cetra_query::tombstone::covered_predicate(
            cetra_query::tombstone::TombstoneTable::Scores,
            &scores_schema,
            &crate::deletion::to_tombstone_specs(&project_deletions),
        )
        .unwrap()
        .unwrap();
        for file in storage.catalog_files(Table::Scores) {
            assert!(
                super::holds_covered(
                    &storage.compactor.merge,
                    &file,
                    &scores_schema,
                    &scores_covered,
                    &project_deletions,
                )
                .await
                .unwrap(),
                "scores file matching only one disjunct must be detected"
            );
        }

        // End-to-end rewrite removes all 4 covered rows and keeps only the uncovered ones.
        let report = storage.apply_deletions(at(11, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (4, 1, 0),
            "{report:?}"
        );
        let spans_files = storage.catalog_files(Table::Spans);
        assert_eq!(spans_files.len(), 1);
        assert_eq!(
            spans_trace_ids(dir.path(), &spans_files[0]),
            vec![trace_bytes(3)]
        );
        let scores = score_file_rows(dir.path(), &storage);
        assert_eq!(scores.len(), 1, "{scores:?}");
        assert!(scores[0].0.ends_with("22"), "{scores:?}");
        assert_eq!(
            status(&storage, &deletion).await,
            DeletionStatus::Applied {
                applied_at: at(11, 0, 0)
            }
        );
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn holds_covered_when_start_time_exceeds_cutoff_but_ingest_time_is_within_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();

        // `start_time` / `created_at` is AFTER `cutoff`, while `ingest_time` is AT/BEFORE `cutoff`.
        let cutoff = at(10, 0, 0);
        let late_event_time = at(12, 0, 0);
        let early_ingest_time = at(9, 0, 0);

        storage = flush_cycle(
            &config,
            storage,
            span_rows(&[
                span_row(1, 0, late_event_time, early_ingest_time),
                span_row(2, 0, late_event_time, early_ingest_time),
            ]),
        )
        .await;
        storage = flush_scores(
            &config,
            storage,
            score_rows(&[
                score_row(
                    0x11,
                    1,
                    late_event_time,
                    late_event_time,
                    early_ingest_time,
                    false,
                    "covered-late-created",
                ),
                score_row(
                    0x22,
                    2,
                    late_event_time,
                    late_event_time,
                    early_ingest_time,
                    false,
                    "kept",
                ),
            ]),
        )
        .await;

        let deletion = traces_deletion(&[1], cutoff);
        storage.register_deletion(deletion.clone()).await.unwrap();

        let tombstones = Arc::clone(&storage.published.load().tombstones);
        let project_deletions = tombstones.for_project(project());
        // Single-target `Traces` deletion must populate a single `trace_id` hint.
        assert_eq!(
            super::single_target_id_lookup(FileTable::Spans, &project_deletions).len(),
            1
        );

        let spans_file = storage.catalog_files(Table::Spans).pop().unwrap();
        let spans_schema = FileTable::Spans.column_schema();
        let spans_covered = cetra_query::tombstone::covered_predicate(
            cetra_query::tombstone::TombstoneTable::Spans,
            &spans_schema,
            &crate::deletion::to_tombstone_specs(&project_deletions),
        )
        .unwrap()
        .unwrap();
        assert!(
            super::holds_covered(
                &storage.compactor.merge,
                &spans_file,
                &spans_schema,
                &spans_covered,
                &project_deletions,
            )
            .await
            .unwrap(),
            "spans file with start_time > cutoff and ingest_time <= cutoff must be covered"
        );

        let scores_file = storage.catalog_files(Table::Scores).pop().unwrap();
        let scores_schema = FileTable::Scores.column_schema();
        let scores_covered = cetra_query::tombstone::covered_predicate(
            cetra_query::tombstone::TombstoneTable::Scores,
            &scores_schema,
            &crate::deletion::to_tombstone_specs(&project_deletions),
        )
        .unwrap()
        .unwrap();
        assert!(
            super::holds_covered(
                &storage.compactor.merge,
                &scores_file,
                &scores_schema,
                &scores_covered,
                &project_deletions,
            )
            .await
            .unwrap(),
            "scores file with created_at > cutoff and ingest_time <= cutoff must be covered"
        );

        let report = storage.apply_deletions(at(13, 0, 0)).await.unwrap();
        assert_eq!(
            (report.rewritten, report.applied, report.failed),
            (2, 1, 0),
            "{report:?}"
        );
        let remaining_spans = storage.catalog_files(Table::Spans);
        assert_eq!(remaining_spans.len(), 1);
        assert_eq!(
            spans_trace_ids(dir.path(), &remaining_spans[0]),
            vec![trace_bytes(2)]
        );
        let remaining_scores = score_file_rows(dir.path(), &storage);
        assert_eq!(remaining_scores.len(), 1);
        assert!(remaining_scores[0].0.ends_with("22"));
        storage.shutdown().await.unwrap();
    }
}
