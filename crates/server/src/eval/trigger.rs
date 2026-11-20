//! The online-rule trigger (D-I-7 to D-I-11): the tracked projects, the flusher, startup and the
//! sweeper, which completes traces, matches each rule through storage, samples, caps and enqueues.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use cetra_core::ids::{OrgId, ProjectId, TraceId};
use cetra_storage::{
    MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE, QueryContext, RunError, SpanRefreshRequest,
    TraceRefreshRequest,
};
use uuid::Uuid;

use crate::api::convert::{MatchRow, match_rows};
use crate::entitlements::{Visibility, query_options};
use crate::eval::hourly::HourlyCaps;
use crate::eval::pending::{Changes, Completed, Entry};
use crate::eval::sampling;
use crate::http::AppState;
use crate::issues::sources::ProjectSources;
use crate::meta::MetaError;
use crate::meta::eval_jobs::{KIND_RULE, NewJob, insert_rule_job};
use crate::meta::issues::SweepSource;
use crate::meta::online_rules::{self, TriggerRule};
use crate::meta::pending_traces::{PendingRow, load_pending, write_pending};
use crate::meta::projects::{
    COMPLETION_TIMEOUT_DEFAULT_S, COMPLETION_TIMEOUT_MAX_S, COMPLETION_TIMEOUT_MIN_S,
};

/// Most `pending_traces` rows written per transaction (D-I-8).
const FLUSH_CHUNK: usize = 1_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The enabled, unpaused rules of one tracked project, and its completion timeout.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectRules {
    /// The project's org, whose data window the reads see (D-P-44).
    pub(crate) org: OrgId,
    pub(crate) timeout_ns: i64,
    pub(crate) rules: Vec<TriggerRule>,
}

/// What one flush wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FlushReport {
    pub(crate) upserts: usize,
    pub(crate) deletes: usize,
}

/// Reads the enabled, unpaused rules and groups them by project. A project's timeout is its stored
/// `trace_completion_timeout_s` when that is in range, else the default. The tracked set is not
/// touched.
///
/// # Errors
///
/// [`MetaError`] when the read fails.
async fn read_rules(state: &AppState) -> Result<HashMap<ProjectId, ProjectRules>, MetaError> {
    #[cfg(test)]
    if faults::take_refresh() {
        return Err(MetaError::Db(sqlx::Error::PoolTimedOut));
    }
    let rules = online_rules::trigger_rules(state.meta.reader()).await?;
    let mut projects: HashMap<ProjectId, ProjectRules> = HashMap::new();
    for rule in rules {
        let timeout_ns = timeout_ns(rule.completion_timeout_s);
        projects
            .entry(rule.project_id)
            .or_insert_with(|| ProjectRules {
                org: rule.org_id,
                timeout_ns,
                rules: Vec::new(),
            })
            .rules
            .push(rule);
    }
    Ok(projects)
}

/// Reads the rules and the error sources, and sets the tracked set to the union of their
/// projects (D-I-8, D-IC-2). Either read failing returns its error before the tracked set is
/// set, so the set stays as it was.
///
/// # Errors
///
/// [`MetaError`] when a read fails.
pub(crate) async fn refresh_tracked(
    state: &AppState,
) -> Result<
    (
        HashMap<ProjectId, ProjectRules>,
        HashMap<ProjectId, ProjectSources>,
    ),
    MetaError,
> {
    let rules = read_rules(state).await?;
    let sources = crate::issues::sources::refresh(state).await?;
    let tracked: HashSet<ProjectId> = rules.keys().chain(sources.keys()).copied().collect();
    state.pending.set_tracked(tracked);
    Ok((rules, sources))
}

/// Startup: the table's rows into the map, after [`refresh_tracked`] (D-I-8). When the rules cannot
/// be read, the rows are still loaded: their projects are tracked for now, and the first sweep's
/// refresh sets the real tracked set and drops the rest.
///
/// # Errors
///
/// [`MetaError`] when the rows cannot be read.
pub(crate) async fn load(state: &AppState) -> Result<(), MetaError> {
    let rows = load_pending(state.meta.reader()).await?;
    if let Err(error) = refresh_tracked(state).await {
        tracing::warn!(
            error = %error,
            "online rules could not be read at startup; stored pending traces are held until the first sweep"
        );
        state
            .pending
            .set_tracked(rows.iter().map(|row| row.project_id).collect());
    }
    state.pending.load(rows.into_iter().map(|row| {
        (
            row.project_id,
            row.trace_id,
            Entry {
                first_seen: row.first_seen,
                last_seen: row.last_seen,
                min_start: row.min_start,
                root_ended: row.root_ended,
                failed_reads: 0,
                spans: 0,
            },
        )
    }));
    Ok(())
}

/// Writes one take of the map's changes (at most [`MAX_CHANGES_PER_TAKE`]) to `pending_traces`
/// (D-I-8). The rest wait for the next tick; [`flush_all`] writes them now. A failed write puts back
/// its own changes and every later part's.
///
/// # Errors
///
/// [`MetaError`] when a write fails.
pub(crate) async fn flush_once(state: &AppState) -> Result<FlushReport, MetaError> {
    let mut parts = flush_parts(state.pending.take_changes(), FLUSH_CHUNK).into_iter();
    let mut report = FlushReport::default();
    while let Some(part) = parts.next() {
        let deletes = part.deletes.clone();
        let upserts: Vec<PendingRow> = part
            .upserts
            .iter()
            .map(|(project_id, trace_id, entry)| PendingRow {
                project_id: *project_id,
                trace_id: *trace_id,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                min_start: entry.min_start,
                root_ended: entry.root_ended,
            })
            .collect();
        let written = (deletes.len(), upserts.len());
        let result = state
            .meta
            .write(move |conn| {
                Box::pin(async move { write_pending(conn, &deletes, &upserts).await })
            })
            .await;
        if let Err(error) = result {
            let mut rest = part;
            for later in parts {
                rest.upserts.extend(later.upserts);
                rest.deletes.extend(later.deletes);
            }
            state.pending.restore(rest);
            return Err(error);
        }
        report.deletes += written.0;
        report.upserts += written.1;
    }
    Ok(report)
}

/// Writes every pending change, one take after another. Shutdown uses it, so the table holds the
/// whole map when the process exits. The periodic flusher writes one take per tick instead.
///
/// # Errors
///
/// [`MetaError`] when a write fails; the unwritten changes are put back.
pub(crate) async fn flush_all(state: &AppState) -> Result<FlushReport, MetaError> {
    let mut total = FlushReport::default();
    loop {
        let report = flush_once(state).await?;
        if report == FlushReport::default() {
            return Ok(total);
        }
        total.upserts += report.upserts;
        total.deletes += report.deletes;
    }
}

/// Cuts `changes` into parts of at most `chunk` rows. The deletes come first, then the upserts.
fn flush_parts(changes: Changes, chunk: usize) -> Vec<Changes> {
    let mut parts = Vec::new();
    let mut current = Changes::default();
    let mut rows = 0;
    for delete in changes.deletes {
        if rows == chunk {
            parts.push(std::mem::take(&mut current));
            rows = 0;
        }
        current.deletes.push(delete);
        rows += 1;
    }
    for upsert in changes.upserts {
        if rows == chunk {
            parts.push(std::mem::take(&mut current));
            rows = 0;
        }
        current.upserts.push(upsert);
        rows += 1;
    }
    if rows > 0 {
        parts.push(current);
    }
    parts
}

/// Most completed traces taken per project per tick (D-I-9).
const PER_PROJECT_TICK: usize = 2_000;
/// Most traces in one batch: one `trace_refresh` or `span_refresh` call.
const BATCH: usize = MAX_REFRESH_IDS;
/// Widest `min_start` spread in one batch, so `around ± 1 day` covers it (D-I-9).
const BATCH_SPREAD_NS: i64 = 12 * 3_600 * NANOS_PER_SECOND;
/// Span rules keep this many sampled spans per trace (D-I-10).
const MAX_SAMPLED_SPANS: usize = 50;
/// Time allowed for one rule's read.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Most jobs inserted per write (D-I-11).
const ENQUEUE_CHUNK: usize = 1_000;

/// What one sweep did. Logged at `debug`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepReport {
    /// Traces taken.
    pub(crate) completed: usize,
    /// Rows the rule reads returned.
    pub(crate) matched: usize,
    /// Targets kept by sampling and the span cut.
    pub(crate) sampled: usize,
    /// Targets refused by the hourly cap.
    pub(crate) capped: usize,
    /// Rule jobs inserted.
    pub(crate) enqueued: usize,
    /// Batches whose reads or writes failed.
    pub(crate) failed_batches: usize,
    /// Traces dropped after too many failed sweeps.
    pub(crate) dropped: usize,
}

/// The matched targets of one batch, before the hourly cap.
#[derive(Debug, Default)]
struct Candidates<'a> {
    /// Rows the reads returned, over every rule.
    matched: usize,
    /// Sampled targets in rule order: the rule they belong to, then the row.
    targets: Vec<(&'a TriggerRule, MatchRow)>,
}

/// One sweep (D-I-9): completes traces, matches each rule through storage, samples, caps and
/// enqueues rule jobs.
///
/// # Errors
///
/// [`MetaError`] only when the rules cannot be read; a failed batch is counted, not returned.
pub(crate) async fn sweep_once(state: &AppState, now: i64) -> Result<SweepReport, MetaError> {
    let (rules, sources) = refresh_tracked(state).await?;
    let mut projects: Vec<ProjectId> = rules.keys().chain(sources.keys()).copied().collect();
    projects.sort();
    projects.dedup();
    let mut report = SweepReport::default();
    for project in projects {
        if state.eval.is_stopped() {
            break;
        }
        let project_rules = rules.get(&project);
        let project_sources = sources.get(&project);
        let Some((org, timeout_ns)) = project_rules
            .map(|found| (found.org, found.timeout_ns))
            .or_else(|| project_sources.map(|found| (found.org, found.timeout_ns)))
        else {
            continue;
        };
        let project_rule_list: &[TriggerRule] = match project_rules {
            Some(found) => &found.rules,
            None => &[],
        };
        let project_source_list: &[SweepSource] = match project_sources {
            Some(found) => &found.sources,
            None => &[],
        };
        let completed = state
            .pending
            .completed(project, now, timeout_ns, PER_PROJECT_TICK);
        report.completed += completed.len();
        let ctx = SweepCtx {
            state,
            org,
            project,
            rules: project_rule_list,
            sources: project_source_list,
            now,
        };
        for (around, batch) in batches(&completed) {
            if state.eval.is_stopped() {
                break;
            }
            let outcome = sweep_batch(&ctx, around, batch, &mut report).await;
            #[cfg(test)]
            faults::run_before_settle();
            settle(state, project, batch, outcome, &mut report);
        }
    }
    if report.enqueued > 0 {
        state.eval.wake();
    }
    Ok(report)
}

/// What one sweep works with for one project: its state, the rules, the error sources, and the
/// sweep's clock.
pub(crate) struct SweepCtx<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) org: OrgId,
    pub(crate) project: ProjectId,
    pub(crate) rules: &'a [TriggerRule],
    pub(crate) sources: &'a [SweepSource],
    pub(crate) now: i64,
}

/// Ends one batch: `Ok` removes its traces that did not grow; `Err` counts a failed read for each
/// and drops those at the limit. The drops are counted in `report`.
fn settle(
    state: &AppState,
    project: ProjectId,
    batch: &[Completed],
    outcome: Result<(), ()>,
    report: &mut SweepReport,
) {
    match outcome {
        Ok(()) => state.pending.finish(project, batch),
        Err(()) => {
            report.failed_batches += 1;
            let dropped = state.pending.read_failed(project, batch);
            if dropped > 0 {
                tracing::warn!(
                    project_id = %project,
                    dropped,
                    "pending traces dropped after 5 failed reads"
                );
                report.dropped += dropped;
            }
        }
    }
}

/// Ends one batch's sweep: the error issues first (D-IC-2), then the rules. The error sweep is
/// idempotent, so a later rule failure that retries the batch repeats nothing that costs a cap.
/// The rules' reads, admission and enqueue come after: see [`read_candidates`]. A project with no
/// rules or no error sources skips its step.
async fn sweep_batch(
    ctx: &SweepCtx<'_>,
    around: i64,
    batch: &[Completed],
    report: &mut SweepReport,
) -> Result<(), ()> {
    if !ctx.sources.is_empty() {
        crate::issues::errors::sweep_batch(ctx, around, batch).await?;
    }
    if ctx.rules.is_empty() {
        return Ok(());
    }
    rule_batch(ctx, around, batch, report).await
}

/// Reads, admits and enqueues one batch's rules: read first, then, chunk by chunk, admit and
/// enqueue. Only the reads can fail before the hourly cap is touched, so a failed read spends no
/// unit and counts nothing. A chunk is admitted just before its write, so a failed write spends the
/// cap of its own chunk only.
async fn rule_batch(
    ctx: &SweepCtx<'_>,
    around: i64,
    batch: &[Completed],
    report: &mut SweepReport,
) -> Result<(), ()> {
    let candidates = read_candidates(ctx, around, batch).await?;
    report.matched += candidates.matched;
    report.sampled += candidates.targets.len();
    let mut targets = candidates.targets;
    while !targets.is_empty() {
        let rest = targets.split_off(targets.len().min(ENQUEUE_CHUNK));
        let chunk = std::mem::replace(&mut targets, rest);
        let jobs = admit_targets(&ctx.state.eval.hourly, chunk, ctx.now, report);
        if !jobs.is_empty() {
            insert_jobs(ctx.state, ctx.project, jobs, ctx.now, report).await?;
        }
    }
    Ok(())
}

/// Every rule's read of one batch, then sampling. No hourly cap and no report: that is what
/// guarantees a failed read spends no cap unit and counts nothing.
async fn read_candidates<'a>(
    ctx: &SweepCtx<'a>,
    around: i64,
    batch: &[Completed],
) -> Result<Candidates<'a>, ()> {
    let SweepCtx {
        state,
        org,
        project,
        rules,
        now,
        ..
    } = *ctx;
    let opened = state
        .storage
        .query(
            project,
            query_options(Visibility::window(state, org), Some(around)),
        )
        .await
        .ok()
        .and_then(|source| QueryContext::new(source, None).ok());
    let Some(ctx) = opened else {
        tracing::warn!(project_id = %project, "a trigger read could not start");
        return Err(());
    };
    let ids: Vec<String> = batch
        .iter()
        .map(|entry| entry.trace_id.to_string())
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut candidates = Candidates::default();
    for rule in rules {
        let span_rule = rule.target == "span";
        let read = if span_rule {
            cetra_storage::span_refresh(
                &ctx,
                SpanRefreshRequest {
                    filter: rule.filter.as_deref(),
                    trace_ids: &refs,
                    per_trace: MAX_REFRESH_SPANS_PER_TRACE,
                    strict_exclusions: true,
                    timeout: READ_TIMEOUT,
                },
            )
            .await
        } else {
            cetra_storage::trace_refresh(
                &ctx,
                TraceRefreshRequest {
                    filter: rule.filter.as_deref(),
                    search: None,
                    trace_ids: &refs,
                    include_eval: false,
                    include_experiments: false,
                    strict_exclusions: true,
                    timeout: READ_TIMEOUT,
                },
            )
            .await
        };
        #[cfg(test)]
        let read = faults::take_read().map_or(read, Err);
        let batches = match read {
            Ok(batches) => batches,
            // The filter does not parse: this rule is skipped, the others go on. The warning is
            // logged at most once per rule per hour.
            Err(RunError::Invalid(_) | RunError::InvalidRequest(_)) => {
                if state.eval.filter_warning_due(rule.id, now) {
                    tracing::warn!(
                        project_id = %project,
                        rule_id = %rule.id,
                        "an online rule's filter could not be read; the rule was skipped"
                    );
                }
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project,
                    rule_id = %rule.id,
                    kind = error.kind(),
                    "a trigger read failed"
                );
                return Err(());
            }
        };
        let rows = match match_rows(&batches, span_rule) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    project_id = %project,
                    rule_id = %rule.id,
                    error = %error,
                    "a trigger read returned a row that does not convert"
                );
                return Err(());
            }
        };
        candidates.matched += rows.len();
        if span_rule {
            // `span_refresh` returns each trace's spans together, in start order.
            let mut trace: Option<TraceId> = None;
            let mut kept = 0;
            for row in rows {
                if trace != Some(row.trace_id) {
                    trace = Some(row.trace_id);
                    kept = 0;
                }
                if kept < MAX_SAMPLED_SPANS
                    && sampling::sampled(rule.sample_rate, rule.id, row.trace_id, row.span_id)
                {
                    kept += 1;
                    candidates.targets.push((rule, row));
                }
            }
        } else {
            for row in rows {
                if sampling::sampled(rule.sample_rate, rule.id, row.trace_id, None) {
                    candidates.targets.push((rule, row));
                }
            }
        }
    }
    Ok(candidates)
}

/// Asks the hourly cap for each target, in order, and builds the jobs of the admitted ones (D-I-11).
/// Called once per chunk, just before the chunk's write.
fn admit_targets(
    hourly: &HourlyCaps,
    targets: Vec<(&TriggerRule, MatchRow)>,
    now: i64,
    report: &mut SweepReport,
) -> Vec<NewJob> {
    let mut jobs = Vec::with_capacity(targets.len());
    for (rule, row) in targets {
        if !hourly.admit(rule.id, now) {
            report.capped += 1;
            continue;
        }
        jobs.push(NewJob {
            id: Uuid::now_v7(),
            project_id: rule.project_id,
            kind: KIND_RULE,
            rule_id: Some(rule.id),
            evaluator_id: rule.evaluator_id,
            variable_mapping: None,
            trace_id: row.trace_id,
            span_id: row.span_id,
            trace_start_ns: row.start_time,
        });
    }
    jobs
}

/// Inserts one chunk of rule jobs in one write (D-I-11). A write error is logged and fails the batch.
async fn insert_jobs(
    state: &AppState,
    project: ProjectId,
    jobs: Vec<NewJob>,
    now: i64,
    report: &mut SweepReport,
) -> Result<(), ()> {
    #[cfg(test)]
    if faults::take_write() {
        tracing::error!(project_id = %project, "rule jobs could not be queued");
        return Err(());
    }
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let mut inserted: usize = 0;
                for job in jobs {
                    if insert_rule_job(conn, job, now).await? {
                        inserted += 1;
                    }
                }
                Ok(inserted)
            })
        })
        .await;
    match written {
        Ok(inserted) => {
            report.enqueued += inserted;
            Ok(())
        }
        Err(error) => {
            tracing::error!(project_id = %project, error = %error, "rule jobs could not be queued");
            Err(())
        }
    }
}

/// Cuts completed traces (sorted by `min_start`) into batches of at most [`BATCH`] traces spanning
/// at most 12 h, each with its `around`: the middle of its range (D-I-9).
fn batches(completed: &[Completed]) -> Vec<(i64, &[Completed])> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < completed.len() {
        let first = completed[start].min_start;
        let mut end = start + 1;
        while end < completed.len()
            && end - start < BATCH
            && completed[end].min_start.saturating_sub(first) <= BATCH_SPREAD_NS
        {
            end += 1;
        }
        let last = completed[end - 1].min_start;
        let around = first.saturating_add(last.saturating_sub(first) / 2);
        out.push((around, &completed[start..end]));
        start = end;
    }
    out
}

/// A stored timeout in seconds as nanoseconds; out of range means the default.
pub(crate) fn timeout_ns(stored: Option<i64>) -> i64 {
    let seconds = stored
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| (COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S).contains(value))
        .unwrap_or(COMPLETION_TIMEOUT_DEFAULT_S);
    i64::from(seconds) * NANOS_PER_SECOND
}

/// Test-only fault injection for the trigger's reads, writes and sweep order. Each fault is armed
/// by a test on its own thread and fires once.
#[cfg(test)]
pub(crate) mod faults {
    use std::cell::{Cell, RefCell};

    use cetra_storage::RunError;

    thread_local! {
        static READ: RefCell<Option<RunError>> = const { RefCell::new(None) };
        static WRITE: Cell<bool> = const { Cell::new(false) };
        static REFRESH: Cell<bool> = const { Cell::new(false) };
        static BEFORE_SETTLE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// The next candidate read fails with `error`.
    pub(super) fn fail_next_read(error: RunError) {
        READ.with(|slot| *slot.borrow_mut() = Some(error));
    }

    /// The next rule-job write fails.
    pub(super) fn fail_next_write() {
        WRITE.with(|flag| flag.set(true));
    }

    /// The next rule read of the trigger fails.
    pub(super) fn fail_next_refresh() {
        REFRESH.with(|flag| flag.set(true));
    }

    /// Runs `hook` after the next batch's reads, before its traces are settled.
    pub(super) fn before_next_settle(hook: impl FnOnce() + 'static) {
        BEFORE_SETTLE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn take_read() -> Option<RunError> {
        READ.with(|slot| slot.borrow_mut().take())
    }

    pub(super) fn take_write() -> bool {
        WRITE.with(|flag| flag.replace(false))
    }

    pub(super) fn take_refresh() -> bool {
        REFRESH.with(|flag| flag.replace(false))
    }

    pub(super) fn run_before_settle() {
        let hook = BEFORE_SETTLE.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_parts_puts_deletes_first_and_cuts_at_the_chunk() {
        let project = ProjectId::new_v7();
        let trace = |byte: u8| TraceId::from_bytes([byte; 16]).unwrap();
        let entry = Entry {
            first_seen: 1,
            last_seen: 1,
            min_start: 1,
            root_ended: false,
            failed_reads: 0,
            spans: 1,
        };
        let changes = Changes {
            upserts: (1..=4).map(|byte| (project, trace(byte), entry)).collect(),
            deletes: (10..=12).map(|byte| (project, trace(byte))).collect(),
        };
        let shape: Vec<(usize, usize)> = flush_parts(changes, 3)
            .iter()
            .map(|part| (part.deletes.len(), part.upserts.len()))
            .collect();
        assert_eq!(shape, vec![(3, 0), (0, 3), (0, 1)]);
    }

    #[test]
    fn timeouts_outside_the_range_mean_the_default() {
        assert_eq!(timeout_ns(None), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(5)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(4_000)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(-1)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(120)), 120 * NANOS_PER_SECOND);
    }

    mod db {
        use std::collections::HashSet;

        use crate::eval::pending::MAX_CHANGES_PER_TAKE;
        use cetra_core::ids::{OrgId, ProjectId, TraceId};
        use uuid::Uuid;

        use super::super::*;
        use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
        use crate::meta::issues::{SourceKind, SourceRow, insert_source, update_source};
        use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
        use crate::meta::pending_traces::{load_pending, write_pending};
        use crate::meta::projects::insert_project;
        use crate::test_support::{TestApp, dataset_span};

        const SECOND: i64 = 1_000_000_000;

        /// A span of the dataset without a parent, so its trace has a root.
        fn rooted_span() -> cetra_core::span::Span {
            let mut span = dataset_span(0);
            span.parent_span_id = None;
            span.environment = None;
            span.metadata.clear();
            span
        }

        /// An enabled or disabled trace rule on `project`, with its evaluator.
        async fn add_rule(app: &TestApp, project: ProjectId, enabled: bool) {
            let evaluator = Uuid::now_v7();
            let rule = Uuid::now_v7();
            let now = app.state.clock.now_ns();
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        insert_evaluator(
                            conn,
                            evaluator,
                            project,
                            "check",
                            EvaluatorFields {
                                name: format!("Same {evaluator}"),
                                config: "{}".to_owned(),
                                output_score: "{}".to_owned(),
                                connection_id: None,
                                secret_enc: None,
                                version: 1,
                            },
                            None,
                            now,
                        )
                        .await?;
                        insert_rule(
                            conn,
                            NewRule {
                                id: rule,
                                project_id: project,
                                evaluator_id: evaluator,
                                target: "trace".to_owned(),
                                created_by: None,
                            },
                            RuleFields {
                                name: format!("Rule {rule}"),
                                filter: None,
                                variable_mapping: "{}".to_owned(),
                                sample_rate: 1.0,
                                budget_usd: None,
                                budget_period: None,
                                enabled,
                            },
                            now,
                        )
                        .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn flush_writes_changes_and_deletes() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            app.state
                .pending
                .set_tracked(HashSet::from([seeded.project.id]));
            app.commit_spans(&seeded.project, vec![rooted_span()]).await;
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 1,
                    deletes: 0
                }
            );
            let rows = load_pending(app.state.meta.reader()).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].root_ended);
            // Ten seconds after the commit the rooted trace is complete; finish removes it.
            let now = app.state.clock.now_ns() + 10 * SECOND;
            let done = app
                .state
                .pending
                .completed(seeded.project.id, now, 60 * SECOND, 10);
            app.state.pending.finish(seeded.project.id, &done);
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 0,
                    deletes: 1
                }
            );
            assert!(
                load_pending(app.state.meta.reader())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                flush_once(&app.state).await.unwrap(),
                FlushReport::default()
            );
        }

        #[tokio::test]
        async fn refresh_rules_tracks_the_projects_of_enabled_rules() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            add_rule(&app, project, false).await;
            assert!(refresh_tracked(&app.state).await.unwrap().0.is_empty());
            add_rule(&app, project, true).await;
            let tracked = refresh_tracked(&app.state).await.unwrap().0;
            assert_eq!(tracked.len(), 1);
            assert_eq!(tracked[&project].rules.len(), 1);
            assert_eq!(tracked[&project].timeout_ns, 60 * SECOND);

            for (settings, seconds) in [
                (r#"{"trace_completion_timeout_s": 120}"#, 120),
                (r#"{"trace_completion_timeout_s": 5}"#, 60),
            ] {
                app.state
                    .meta
                    .write(move |conn| {
                        Box::pin(async move {
                            sqlx::query("UPDATE projects SET settings = ? WHERE id = ?")
                                .bind(settings)
                                .bind(project.to_string())
                                .execute(&mut *conn)
                                .await?;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap();
                let tracked = refresh_tracked(&app.state).await.unwrap().0;
                assert_eq!(tracked[&project].timeout_ns, seconds * SECOND);
            }
        }

        #[tokio::test]
        async fn load_restores_tracked_entries_and_deletes_the_rest() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let now = app.state.clock.now_ns();
            let org: OrgId = seeded.org_id;
            let tracked_project = seeded.project.id;
            add_rule(&app, tracked_project, true).await;
            let other = app
                .state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        Ok(insert_project(conn, org, "Other", "other", None, now)
                            .await?
                            .id)
                    })
                })
                .await
                .unwrap();
            let row = |project: ProjectId, byte: u8| PendingRow {
                project_id: project,
                trace_id: TraceId::from_bytes([byte; 16]).unwrap(),
                first_seen: now,
                last_seen: now,
                min_start: 0,
                root_ended: false,
            };
            let rows = vec![row(tracked_project, 1), row(other, 2)];
            app.state
                .meta
                .write(move |conn| Box::pin(async move { write_pending(conn, &[], &rows).await }))
                .await
                .unwrap();

            load(&app.state).await.unwrap();
            assert_eq!(app.state.pending.len(), 1);
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 0,
                    deletes: 1
                }
            );
            let left = load_pending(app.state.meta.reader()).await.unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].project_id, tracked_project);
        }

        #[tokio::test]
        async fn load_holds_the_rows_when_the_rules_cannot_be_read() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let now = app.state.clock.now_ns();
            let row = PendingRow {
                project_id: seeded.project.id,
                trace_id: TraceId::from_bytes([1; 16]).unwrap(),
                first_seen: now,
                last_seen: now,
                min_start: 0,
                root_ended: false,
            };
            app.state
                .meta
                .write(move |conn| Box::pin(async move { write_pending(conn, &[], &[row]).await }))
                .await
                .unwrap();

            faults::fail_next_refresh();
            load(&app.state).await.unwrap();
            // The project is tracked until the rules can be read, so the row is held and not deleted.
            assert_eq!(app.state.pending.len(), 1);
            assert_eq!(
                flush_once(&app.state).await.unwrap(),
                FlushReport::default()
            );

            // The first refresh that works tracks no project (no rules exist), and drops the row.
            refresh_tracked(&app.state).await.unwrap();
            assert_eq!(app.state.pending.len(), 0);
            assert_eq!(
                flush_once(&app.state).await.unwrap(),
                FlushReport {
                    upserts: 0,
                    deletes: 1
                }
            );
        }

        #[tokio::test]
        async fn flush_all_writes_every_take() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            app.state.pending.set_tracked(HashSet::from([project]));
            let total = MAX_CHANGES_PER_TAKE + 5;
            let spans: Vec<cetra_core::span::Span> = (0..total)
                .map(|index| {
                    let mut span = rooted_span();
                    let mut bytes = [0u8; 16];
                    bytes[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
                    span.trace_id = TraceId::from_bytes(bytes).unwrap();
                    span
                })
                .collect();
            app.state.pending.observe(project, &spans, 0);

            // The periodic flush writes one take; shutdown's flush writes the rest.
            let first = flush_once(&app.state).await.unwrap();
            assert_eq!(first.upserts, MAX_CHANGES_PER_TAKE);
            let rest = flush_all(&app.state).await.unwrap();
            assert_eq!(rest.upserts, 5);
            assert_eq!(
                load_pending(app.state.meta.reader()).await.unwrap().len(),
                total
            );
        }

        /// An error source of `project`, stored enabled or not.
        async fn error_source(app: &TestApp, project: ProjectId, enabled: bool) -> SourceRow {
            let now = app.state.clock.now_ns();
            let row = SourceRow {
                id: Uuid::now_v7(),
                project_id: project,
                name: format!("Errors {}", Uuid::now_v7()),
                kind: SourceKind::Error,
                filter: None,
                enabled,
                paused_reason: None,
                paused_at: None,
                created_by: None,
                created_at: now,
                updated_at: now,
            };
            let stored = row.clone();
            app.state
                .meta
                .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
                .await
                .unwrap();
            stored
        }

        #[tokio::test]
        async fn project_with_only_an_error_source_is_tracked() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, true).await;
            refresh_tracked(&app.state).await.unwrap();
            app.state.pending.observe(project, &[rooted_span()], 0);
            assert_eq!(app.state.pending.len(), 1);
        }

        #[tokio::test]
        async fn a_disabled_source_untracks_its_project() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let mut source = error_source(&app, project, true).await;
            refresh_tracked(&app.state).await.unwrap();
            app.state.pending.observe(project, &[rooted_span()], 0);
            assert_eq!(app.state.pending.len(), 1);

            source.enabled = false;
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        update_source(conn, &source).await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            let (rules, sources) = refresh_tracked(&app.state).await.unwrap();
            assert!(rules.is_empty());
            assert!(sources.is_empty());
            // The project's held traces are dropped with it.
            assert_eq!(app.state.pending.len(), 0);
            app.state.pending.observe(project, &[rooted_span()], 0);
            assert_eq!(app.state.pending.len(), 0);
        }
    }

    mod sweep {
        use std::time::{Duration, Instant};

        use axum::http::{Method, header};
        use cetra_core::enums::{PayloadFormat, SpanKind, SpanStatus};
        use cetra_core::ids::{ProjectId, SpanId, TraceId};
        use cetra_core::span::{Span, SpanEvent};
        use serde_json::{Value, json};
        use uuid::Uuid;

        use super::super::*;
        use crate::api::convert::MatchRow;
        use crate::eval::EvalTiming;
        use crate::eval::hourly::{HOURLY_CAP, HourlyCaps};
        use crate::eval::pending::Entry;
        use crate::eval::sampling;
        use crate::meta::eval_jobs::KIND_RULE;
        use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
        use crate::meta::issues::{SourceKind, SourceRow, insert_source};
        use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
        use crate::test_support::mock_llm::{MockLlm, completion};
        use crate::test_support::{Seeded, TestApp, capture_logs, dataset_span};

        const SECOND: i64 = 1_000_000_000;
        const HOUR: i64 = 3_600 * SECOND;
        const QUESTION: &str = "What is the refund window?";
        const JUDGE_ANSWER: &str = r#"{"explanation": "Relevant.", "value": 0.9}"#;

        /// The manual clock, moved to 30 minutes past a UTC hour, so no test crosses an hour
        /// boundary. Returns the new time.
        fn set_clock(app: &TestApp) -> i64 {
            let now = app.state.clock.now_ns();
            let at = now - now.rem_euclid(HOUR) + HOUR / 2;
            app.state.clock.set(at);
            at
        }

        fn real_now() -> i64 {
            i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("after the epoch")
                    .as_nanos(),
            )
            .expect("fits i64")
        }

        fn trace_id(n: u128) -> TraceId {
            TraceId::from_bytes(*Uuid::from_u128(n).as_bytes()).expect("not all zero")
        }

        fn span_id(n: u64) -> SpanId {
            SpanId::from_bytes(n.to_be_bytes()).expect("not all zero")
        }

        /// A chain span of `trace`, span `id`, starting at `start`. `root` makes it a span without
        /// a parent; otherwise it is a child of span 1.
        fn span(trace: TraceId, id: u64, root: bool, start: i64) -> Span {
            let mut row = dataset_span(0);
            row.trace_id = trace;
            row.span_id = span_id(id);
            row.parent_span_id = if root { None } else { Some(span_id(1)) };
            row.start_time = start;
            row.end_time = start + 10_000_000;
            row.environment = None;
            row.metadata.clear();
            row.status = SpanStatus::Ok;
            row.span_kind = SpanKind::Chain;
            row
        }

        /// A rule of `target` on `project` (no evaluation runs: the sweeper only enqueues), created
        /// in one write, then `refresh_tracked`, so the project is tracked before any commit.
        async fn rule(
            app: &TestApp,
            project: ProjectId,
            target: &str,
            filter: Option<&str>,
            rate: f64,
        ) -> Uuid {
            let evaluator = Uuid::now_v7();
            let rule = Uuid::now_v7();
            let now = app.state.clock.now_ns();
            let target = target.to_owned();
            let filter = filter.map(str::to_owned);
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        insert_evaluator(
                            conn,
                            evaluator,
                            project,
                            "check",
                            EvaluatorFields {
                                name: format!("Same {evaluator}"),
                                config: "{}".to_owned(),
                                output_score: "{}".to_owned(),
                                connection_id: None,
                                secret_enc: None,
                                version: 1,
                            },
                            None,
                            now,
                        )
                        .await?;
                        insert_rule(
                            conn,
                            NewRule {
                                id: rule,
                                project_id: project,
                                evaluator_id: evaluator,
                                target,
                                created_by: None,
                            },
                            RuleFields {
                                name: format!("Rule {rule}"),
                                filter,
                                variable_mapping: "{}".to_owned(),
                                sample_rate: rate,
                                budget_usd: None,
                                budget_period: None,
                                enabled: true,
                            },
                            now,
                        )
                        .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            refresh_tracked(&app.state).await.unwrap();
            rule
        }

        async fn set_timeout(app: &TestApp, project: ProjectId, seconds: i64) {
            let settings = format!(r#"{{"trace_completion_timeout_s": {seconds}}}"#);
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        sqlx::query("UPDATE projects SET settings = ? WHERE id = ?")
                            .bind(settings)
                            .bind(project.to_string())
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }

        /// One row of `eval_jobs`.
        #[derive(sqlx::FromRow, Debug)]
        struct Job {
            kind: String,
            status: String,
            priority: i64,
            dedup_key: String,
            span_id: Option<Vec<u8>>,
            trace_id: Vec<u8>,
            trace_start_ns: i64,
        }

        async fn jobs(app: &TestApp) -> Vec<Job> {
            sqlx::query_as(
                "SELECT kind, status, priority, dedup_key, span_id, trace_id, trace_start_ns \
                 FROM eval_jobs ORDER BY dedup_key",
            )
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap()
        }

        fn completed(min_start: i64, n: u128) -> Completed {
            Completed {
                trace_id: trace_id(n),
                last_seen: 0,
                min_start,
                spans: 1,
            }
        }

        fn rule_of(n: u128) -> TriggerRule {
            TriggerRule {
                id: Uuid::from_u128(n),
                project_id: ProjectId::new_v7(),
                org_id: OrgId::new_v7(),
                evaluator_id: Uuid::from_u128(100),
                target: "trace".to_owned(),
                filter: None,
                sample_rate: 1.0,
                completion_timeout_s: None,
            }
        }

        #[test]
        fn batches_cut_at_500_and_12_hours() {
            let same: Vec<Completed> = (1..=1_200).map(|n| completed(0, n)).collect();
            let sizes: Vec<usize> = batches(&same)
                .iter()
                .map(|(_, batch)| batch.len())
                .collect();
            assert_eq!(sizes, vec![500, 500, 200]);

            let spread = [
                completed(0, 1),
                completed(6 * HOUR, 2),
                completed(13 * HOUR, 3),
            ];
            let cut = batches(&spread);
            let arounds: Vec<i64> = cut.iter().map(|(around, _)| *around).collect();
            assert_eq!(arounds, vec![3 * HOUR, 13 * HOUR]);
            assert_eq!(cut[0].1.len(), 2);
            assert_eq!(cut[1].1.len(), 1);
        }

        #[test]
        fn admit_targets_caps_in_order() {
            let caps = HourlyCaps::default();
            let now = 12 * HOUR;
            let first = rule_of(1);
            let second = rule_of(2);
            for _ in 0..(HOURLY_CAP - 1) {
                assert!(caps.admit(first.id, now));
            }
            let row = |n: u128| MatchRow {
                trace_id: trace_id(n),
                span_id: None,
                start_time: 5,
            };
            let targets = vec![
                (&first, row(1)),
                (&first, row(2)),
                (&first, row(3)),
                (&second, row(4)),
            ];
            let mut report = SweepReport::default();
            let jobs = admit_targets(&caps, targets, now, &mut report);
            let rules: Vec<Option<Uuid>> = jobs.iter().map(|job| job.rule_id).collect();
            assert_eq!(rules, vec![Some(first.id), Some(second.id)]);
            assert_eq!(report.capped, 2);
        }

        #[tokio::test]
        async fn a_completed_trace_enqueues_a_trace_rule_job() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(&seeded.project, vec![span(trace_id(1), 1, true, start)])
                .await;
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.enqueued, 1);
            let rows = jobs(&app).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            let job = &rows[0];
            assert_eq!(job.kind, KIND_RULE);
            assert_eq!(job.status, "queued");
            assert_eq!(job.priority, 1);
            assert_eq!(job.dedup_key, format!("rule:{rule_id}:{}:-", trace_id(1)));
            assert_eq!(job.trace_start_ns, start);
            assert_eq!(job.trace_id, trace_id(1).as_bytes().to_vec());
            assert_eq!(app.state.pending.len(), 0);
        }

        #[tokio::test]
        async fn an_unrooted_trace_waits_for_the_timeout() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            set_timeout(&app, project, 10).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 2, false, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let early = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(early.enqueued, 0);
            assert_eq!(app.state.pending.len(), 1);
            app.state.clock.advance(Duration::from_secs(5));
            let late = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(late.enqueued, 1);
            assert_eq!(jobs(&app).await.len(), 1);
        }

        #[tokio::test]
        async fn the_filter_selects_traces() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "trace", Some("status = 'error'"), 1.0).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let ok = span(trace_id(1), 1, true, start);
            let mut error = span(trace_id(2), 1, true, start);
            error.status = SpanStatus::Error;
            app.commit_spans(&seeded.project, vec![ok, error]).await;
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.enqueued, 1);
            let rows = jobs(&app).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].trace_id, trace_id(2).as_bytes().to_vec());
            assert_eq!(
                rows[0].dedup_key,
                format!("rule:{rule_id}:{}:-", trace_id(2))
            );
            assert_eq!(app.state.pending.len(), 0, "both traces left the map");
        }

        #[tokio::test]
        async fn a_span_rule_enqueues_matching_spans() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "span", Some("span_kind = 'llm'"), 1.0).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let mut spans = vec![span(trace_id(1), 1, true, start)];
            for id in 2..=4 {
                let mut child = span(trace_id(1), id, false, start + id as i64 * 1_000_000);
                child.span_kind = SpanKind::Llm;
                spans.push(child);
            }
            spans.push(span(trace_id(1), 5, false, start + 5_000_000));
            app.commit_spans(&seeded.project, spans).await;
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.enqueued, 3);
            let mut span_ids: Vec<Vec<u8>> = jobs(&app)
                .await
                .into_iter()
                .map(|job| job.span_id.expect("a span rule's job has a span"))
                .collect();
            span_ids.sort();
            let expected: Vec<Vec<u8>> =
                (2..=4).map(|id| span_id(id).as_bytes().to_vec()).collect();
            assert_eq!(span_ids, expected);
        }

        #[tokio::test]
        async fn a_span_rule_keeps_the_50_earliest_sampled_spans() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "span", Some("span_kind = 'llm'"), 1.0).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let mut spans = vec![span(trace_id(1), 1, true, start)];
            for id in 2..=61u64 {
                let mut child = span(trace_id(1), id, false, start + id as i64 * 1_000_000);
                child.span_kind = SpanKind::Llm;
                spans.push(child);
            }
            app.commit_spans(&seeded.project, spans).await;
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.matched, 60);
            assert_eq!(report.enqueued, 50);
            let mut span_ids: Vec<Vec<u8>> = jobs(&app)
                .await
                .into_iter()
                .map(|job| job.span_id.expect("a span"))
                .collect();
            span_ids.sort();
            // Spans 2 to 51 start earliest.
            let expected: Vec<Vec<u8>> =
                (2..=51).map(|id| span_id(id).as_bytes().to_vec()).collect();
            assert_eq!(span_ids, expected);
        }

        #[tokio::test]
        async fn sampling_selects_the_same_targets_as_sampled() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "trace", None, 0.5).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let spans: Vec<Span> = (1..=20u128)
                .map(|n| span(trace_id(n), 1, true, start))
                .collect();
            app.commit_spans(&seeded.project, spans).await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let mut actual: Vec<Vec<u8>> = jobs(&app)
                .await
                .into_iter()
                .map(|job| job.trace_id)
                .collect();
            actual.sort();
            let mut expected: Vec<Vec<u8>> = (1..=20u128)
                .filter(|n| sampling::sampled(0.5, rule_id, trace_id(*n), None))
                .map(|n| trace_id(n).as_bytes().to_vec())
                .collect();
            expected.sort();
            assert!(!expected.is_empty());
            assert_eq!(actual, expected);
        }

        #[tokio::test]
        async fn evaluation_traces_are_never_enqueued() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(
                &app,
                project,
                "trace",
                Some("environment = 'cetra-eval'"),
                1.0,
            )
            .await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let mut eval_span = span(trace_id(1), 1, true, start);
            eval_span.environment = Some(cetra_core::names::environment::EVAL.to_owned());
            app.commit_spans(&seeded.project, vec![eval_span]).await;
            assert_eq!(
                app.state.pending.len(),
                0,
                "evaluation spans are not recorded"
            );
            // Forced into the map, the strict read still finds nothing to enqueue.
            app.state.pending.load(vec![(
                project,
                trace_id(9),
                Entry {
                    first_seen: at,
                    last_seen: at,
                    min_start: start,
                    root_ended: true,
                    failed_reads: 0,
                    spans: 1,
                },
            )]);
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.enqueued, 0);
            assert!(jobs(&app).await.is_empty());
            assert_eq!(app.state.pending.len(), 0);
        }

        #[tokio::test]
        async fn the_hourly_cap_drops_and_counts() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            for _ in 0..HOURLY_CAP {
                app.state.eval.hourly.admit(rule_id, now);
            }
            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.enqueued, 0);
            assert_eq!(report.capped, 1);
            assert!(jobs(&app).await.is_empty());
            assert_eq!(app.state.eval.hourly.dropped(rule_id, now), 1);

            let response = app
                .request(
                    Method::GET,
                    &format!("/api/v1/projects/{project}/online-rules/{rule_id}"),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .send()
                .await;
            let body: Value = response.json();
            assert_eq!(body["dropped_last_hour"], 1, "{body}");
        }

        #[tokio::test]
        async fn a_trace_completed_twice_gets_one_job() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let first = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(first.enqueued, 1);

            // The trace grows again and completes again: the same dedup key, so no second job.
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 2, true, at - 20 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let second = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(second.enqueued, 0);
            assert_eq!(jobs(&app).await.len(), 1);
        }

        #[tokio::test]
        async fn a_rule_with_an_unreadable_filter_is_skipped() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let broken = rule(&app, project, "trace", None, 1.0).await;
            let good = rule(&app, project, "trace", None, 1.0).await;
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        sqlx::query("UPDATE online_rules SET filter = ? WHERE id = ?")
                            .bind("status = = 'x'")
                            .bind(broken.to_string())
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            let (report, logs) = capture_logs(sweep_once(&app.state, now)).await;
            let report = report.unwrap();
            assert_eq!(report.enqueued, 1);
            let rows = jobs(&app).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert!(rows[0].dedup_key.starts_with(&format!("rule:{good}:")));
            assert!(logs.contains(&broken.to_string()), "{logs}");
            assert!(logs.contains("was skipped"), "{logs}");
            assert_eq!(app.state.pending.len(), 0);
        }

        #[tokio::test]
        async fn a_failed_batch_keeps_its_traces() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let done =
                app.state
                    .pending
                    .completed(project, app.state.clock.now_ns(), 60 * SECOND, 10);
            assert_eq!(done.len(), 1);

            let mut report = SweepReport::default();
            for _ in 0..4 {
                settle(&app.state, project, &done, Err(()), &mut report);
            }
            assert_eq!(report.failed_batches, 4);
            assert_eq!(report.dropped, 0);
            assert_eq!(app.state.pending.len(), 1);
            settle(&app.state, project, &done, Err(()), &mut report);
            assert_eq!(report.failed_batches, 5);
            assert_eq!(report.dropped, 1);
            assert_eq!(app.state.pending.len(), 0);

            // A batch that succeeds removes its traces.
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(2), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let done =
                app.state
                    .pending
                    .completed(project, app.state.clock.now_ns(), 60 * SECOND, 10);
            settle(&app.state, project, &done, Ok(()), &mut report);
            assert_eq!(app.state.pending.len(), 0);
            assert_eq!(report.failed_batches, 5, "a success counts no failure");
        }

        #[tokio::test]
        async fn a_deleted_rule_during_the_sweep_enqueues_nothing() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let job = NewJob {
                id: Uuid::now_v7(),
                project_id: project,
                kind: KIND_RULE,
                rule_id: Some(Uuid::now_v7()),
                evaluator_id: Uuid::now_v7(),
                variable_mapping: None,
                trace_id: trace_id(1),
                span_id: None,
                trace_start_ns: 0,
            };
            let mut report = SweepReport::default();
            let now = app.state.clock.now_ns();
            insert_jobs(&app.state, project, vec![job], now, &mut report)
                .await
                .unwrap();
            assert_eq!(report.enqueued, 0);
            assert!(jobs(&app).await.is_empty());
        }

        #[tokio::test]
        async fn untracked_projects_are_not_read() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            assert_eq!(app.state.pending.len(), 0);
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report, SweepReport::default());
        }

        #[tokio::test]
        async fn a_failed_candidate_read_spends_no_cap_and_counts_nothing() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            faults::fail_next_read(RunError::Timeout);
            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.failed_batches, 1);
            assert_eq!(report.matched, 0);
            assert_eq!(report.sampled, 0);
            assert_eq!(report.capped, 0);
            assert_eq!(report.enqueued, 0);
            assert!(jobs(&app).await.is_empty());
            assert_eq!(app.state.pending.len(), 1, "the trace stays pending");
            // Nothing was admitted: the whole hour's cap is still free.
            assert!((0..HOURLY_CAP).all(|_| app.state.eval.hourly.admit(rule_id, now)));
            assert_eq!(app.state.eval.hourly.dropped(rule_id, now), 0);
        }

        #[tokio::test]
        async fn a_span_committed_before_the_finish_keeps_its_trace() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            let state = app.state.clone();
            faults::before_next_settle(move || {
                // A commit lands after the read and before `finish`, at the same clock.
                state.pending.observe(
                    project,
                    &[span(trace_id(1), 2, false, at - 20 * SECOND)],
                    now,
                );
            });
            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.enqueued, 1);
            assert_eq!(app.state.pending.len(), 1, "the grown trace stays pending");

            // Idle again, the next sweep completes it.
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(app.state.pending.len(), 0);
        }

        #[tokio::test]
        async fn a_stopped_sweep_takes_no_batch() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![span(trace_id(1), 1, true, at - 30 * SECOND)],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            app.state.eval.stop();
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report, SweepReport::default());
            assert_eq!(app.state.pending.len(), 1);
            assert!(jobs(&app).await.is_empty());
        }

        #[tokio::test]
        async fn an_unreadable_filter_is_warned_once_an_hour() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let broken = rule(&app, project, "trace", None, 1.0).await;
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        sqlx::query("UPDATE online_rules SET filter = ? WHERE id = ?")
                            .bind("status = = 'x'")
                            .bind(broken.to_string())
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            let at = set_clock(&app);
            // Two sweeps in one hour, then one in the next hour.
            let mut warnings = Vec::new();
            for (index, offset) in [0, 60 * SECOND, HOUR].into_iter().enumerate() {
                let at = at + offset;
                app.state.clock.set(at);
                app.commit_spans(
                    &seeded.project,
                    vec![span(trace_id(index as u128 + 1), 1, true, at - 30 * SECOND)],
                )
                .await;
                app.state.clock.advance(Duration::from_secs(6));
                let (report, logs) =
                    capture_logs(sweep_once(&app.state, app.state.clock.now_ns())).await;
                report.unwrap();
                warnings.push(logs.matches("was skipped").count());
            }
            assert_eq!(warnings, vec![1, 0, 1]);
        }

        #[tokio::test]
        async fn a_failed_chunk_spends_only_its_own_cap() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let rule_id = rule(&app, project, "span", Some("span_kind = 'llm'"), 1.0).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            // 500 traces with three matching spans each: one batch of 1 500 targets in two chunks.
            let mut spans = Vec::new();
            for n in 1..=500u128 {
                spans.push(span(trace_id(n), 1, true, start));
                for id in 2..=4u64 {
                    let mut child = span(trace_id(n), id, false, start + id as i64 * 1_000_000);
                    child.span_kind = SpanKind::Llm;
                    spans.push(child);
                }
            }
            app.commit_spans(&seeded.project, spans).await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            faults::fail_next_write();
            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.matched, 1_500);
            assert_eq!(report.failed_batches, 1);
            assert_eq!(report.enqueued, 0);
            // The first chunk of 1 000 was admitted before its failed write. The second was never
            // admitted, so 9 000 units are still free.
            assert!((0..(HOURLY_CAP - 1_000)).all(|_| app.state.eval.hourly.admit(rule_id, now)));
            assert!(!app.state.eval.hourly.admit(rule_id, now));
        }

        /// A judge connection to `mock`, created over HTTP.
        async fn http_connection(app: &TestApp, seeded: &Seeded, mock: &MockLlm) -> Uuid {
            let response = app
                .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&json!({
                    "name": "Mocked",
                    "adapter": "openai",
                    "api_key": "sk-test-key-0123456789",
                    "base_url": mock.base_url(),
                    "default_model": "gpt-test",
                    "max_concurrency": 4,
                    "requests_per_minute": 600
                }))
                .send()
                .await;
            assert_eq!(
                response.status,
                201,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
            let value: Value = response.json();
            Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
        }

        /// A numeric judge named `Relevance`, created over HTTP.
        async fn http_judge(app: &TestApp, seeded: &Seeded, connection: Uuid) -> Uuid {
            let response = app
                .request(
                    Method::POST,
                    &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&json!({
                    "name": "Relevance",
                    "kind": "llm_judge",
                    "config": {
                        "connection_id": connection,
                        "model": "gpt-test",
                        "prompt": "Question: {{ input }}\nAnswer: {{ output }}",
                        "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                        "include_reason": true,
                        "temperature": null,
                        "max_output_tokens": 1024,
                        "timeout_ms": 60000
                    },
                    "output_score": {"name": "relevance", "data_type": "numeric"}
                }))
                .send()
                .await;
            assert_eq!(
                response.status,
                201,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
            let value: Value = response.json();
            Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
        }

        /// An enabled trace rule that runs `evaluator` on every trace, created over HTTP.
        async fn http_rule(app: &TestApp, seeded: &Seeded, evaluator: Uuid) -> Uuid {
            let response = app
                .request(
                    Method::POST,
                    &format!("/api/v1/projects/{}/online-rules", seeded.project.id),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&json!({
                    "name": "Relevance on traces",
                    "evaluator_id": evaluator,
                    "target": "trace",
                    "variable_mapping": {"input": "trace.input", "output": "trace.output"},
                    "sample_rate": 1.0,
                    "enabled": true
                }))
                .send()
                .await;
            assert_eq!(
                response.status,
                201,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
            let value: Value = response.json();
            Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_trace_rule_runs_from_commit_to_score() {
            let app = TestApp::with_config(|config| {
                config.eval.workers = 4;
                config.security.allow_private_network_egress = true;
            })
            .await;
            app.state.clock.set(real_now());
            let seeded = app.seed().await;
            let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
            let connection = http_connection(&app, &seeded, &mock).await;
            let evaluator = http_judge(&app, &seeded, connection).await;
            let rule_id = http_rule(&app, &seeded, evaluator).await;
            refresh_tracked(&app.state).await.unwrap();
            let tasks = crate::eval::spawn(&app.state, EvalTiming::TEST);

            let now = app.state.clock.now_ns();
            let mut row = span(trace_id(1), 1, true, now - 30 * SECOND);
            row.environment = Some("production".to_owned());
            row.input = Some(QUESTION.to_owned());
            row.input_format = Some(PayloadFormat::Text);
            row.output = Some("30 days.".to_owned());
            row.output_format = Some(PayloadFormat::Text);
            app.commit_spans(&seeded.project, vec![row]).await;
            app.state.clock.advance(Duration::from_secs(6));

            let deadline = Instant::now() + Duration::from_secs(10);
            let finished = loop {
                let rows = jobs(&app).await;
                if let Some(job) = rows
                    .iter()
                    .find(|job| matches!(job.status.as_str(), "done" | "failed"))
                {
                    break job.status.clone();
                }
                assert!(Instant::now() < deadline, "no job finished: {rows:?}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_eq!(finished, "done");
            let rows = jobs(&app).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert!(rows[0].dedup_key.starts_with(&format!("rule:{rule_id}:")));

            let scores = app
                .request(
                    Method::GET,
                    &format!(
                        "/api/v1/projects/{}/scores?rule_id={rule_id}",
                        seeded.project.id
                    ),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .send()
                .await;
            let page: Value = scores.json();
            assert_eq!(page["data"].as_array().expect("data").len(), 1, "{page}");
            tasks.shutdown(&app.state).await;
        }

        /// An enabled error source of `project` with `filter`. Returns its id.
        async fn error_source(app: &TestApp, project: ProjectId, filter: Option<&str>) -> Uuid {
            let now = app.state.clock.now_ns();
            let row = SourceRow {
                id: Uuid::now_v7(),
                project_id: project,
                name: format!("Errors {}", Uuid::now_v7()),
                kind: SourceKind::Error,
                filter: filter.map(str::to_owned),
                enabled: true,
                paused_reason: None,
                paused_at: None,
                created_by: None,
                created_at: now,
                updated_at: now,
            };
            let id = row.id;
            app.state
                .meta
                .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
                .await
                .unwrap();
            refresh_tracked(&app.state).await.unwrap();
            id
        }

        /// A root error span of `trace`, starting at `start`, from `service`, with an `exception`
        /// event of `kind` and `message`.
        fn error_span(
            trace: TraceId,
            start: i64,
            service: &str,
            kind: &str,
            message: &str,
        ) -> Span {
            let mut row = span(trace, 1, true, start);
            row.status = SpanStatus::Error;
            row.service_name = Some(service.to_owned());
            row.events = vec![SpanEvent {
                time: start,
                name: "exception".to_owned(),
                attributes: Some(
                    json!({"exception.type": kind, "exception.message": message}).to_string(),
                ),
            }];
            row
        }

        /// The issues of the project: title, state, regressed flag, first and last seen.
        async fn issues_of(app: &TestApp) -> Vec<(String, String, i64, i64, i64)> {
            sqlx::query_as(
                "SELECT title, state, regressed, first_seen, last_seen FROM issues ORDER BY title",
            )
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap()
        }

        /// The events of the issue titled `title`, oldest first: kind, from and to values.
        async fn events_of(
            app: &TestApp,
            title: &str,
        ) -> Vec<(String, Option<String>, Option<String>)> {
            sqlx::query_as(
                "SELECT e.kind, e.from_value, e.to_value FROM issue_events e \
                 JOIN issues i ON i.id = e.issue_id WHERE i.title = ? ORDER BY e.at, e.id",
            )
            .bind(title)
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap()
        }

        /// Runs `statement` with the binds in one write.
        async fn exec(app: &TestApp, statement: &'static str, binds: Vec<String>) {
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        let mut query = sqlx::query(statement);
                        for bind in binds {
                            query = query.bind(bind);
                        }
                        query.execute(&mut *conn).await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn error_span_becomes_an_issue() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    start,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let title = "ValueError: bad id <n>";
            assert_eq!(
                issues_of(&app).await,
                vec![(title.to_owned(), "open".to_owned(), 0, start, start)]
            );
            assert_eq!(
                events_of(&app, title).await,
                vec![("created".to_owned(), None, Some("open".to_owned()))]
            );
        }

        #[tokio::test]
        async fn same_error_in_two_traces_is_one_issue() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(trace_id(1), start, "api", "ValueError", "bad id 42"),
                    error_span(
                        trace_id(2),
                        start + SECOND,
                        "api",
                        "ValueError",
                        "bad id 43",
                    ),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(
                issues_of(&app).await,
                vec![(
                    "ValueError: bad id <n>".to_owned(),
                    "open".to_owned(),
                    0,
                    start,
                    start + SECOND
                )]
            );
        }

        #[tokio::test]
        async fn different_exception_types_are_two_issues() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(trace_id(1), start, "api", "ValueError", "bad id 42"),
                    error_span(trace_id(2), start, "api", "KeyError", "bad id 42"),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let titles: Vec<String> = issues_of(&app).await.into_iter().map(|row| row.0).collect();
            assert_eq!(
                titles,
                vec![
                    "KeyError: bad id <n>".to_owned(),
                    "ValueError: bad id <n>".to_owned()
                ]
            );
        }

        #[tokio::test]
        async fn resolved_issue_regresses_on_a_newer_trace() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let title = "ValueError: bad id <n>";
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 60 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 1",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            // Resolved 30 seconds before the clock's start time.
            exec(
                &app,
                "UPDATE issues SET state = 'resolved', resolved_at = ? WHERE title = ?",
                vec![(at - 30 * SECOND).to_string(), title.to_owned()],
            )
            .await;

            // A trace that started before the resolution, but completes after it: no reopen.
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(2),
                    at - 50 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 2",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(issues_of(&app).await[0].1, "resolved");
            assert_eq!(issues_of(&app).await[0].2, 0);

            // A trace that started after the resolution reopens the issue.
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(3),
                    at - 10 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 3",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(issues_of(&app).await[0].1, "open");
            assert_eq!(issues_of(&app).await[0].2, 1);
            assert_eq!(
                events_of(&app, title).await,
                vec![
                    ("created".to_owned(), None, Some("open".to_owned())),
                    (
                        "regressed".to_owned(),
                        Some("resolved".to_owned()),
                        Some("open".to_owned())
                    ),
                ]
            );
        }

        #[tokio::test]
        async fn merged_issue_routes_to_its_target() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(trace_id(1), start, "api", "ValueError", "bad id 42"),
                    error_span(trace_id(2), start, "api", "KeyError", "missing key"),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            exec(
                &app,
                "UPDATE issues SET state = 'merged', \
                 merged_into = (SELECT id FROM issues WHERE title = ?) WHERE title = ?",
                vec![
                    "ValueError: bad id <n>".to_owned(),
                    "KeyError: missing key".to_owned(),
                ],
            )
            .await;

            let later = start + 2 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(3),
                    later,
                    "api",
                    "KeyError",
                    "missing key",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(
                issues_of(&app).await,
                vec![
                    (
                        "KeyError: missing key".to_owned(),
                        "merged".to_owned(),
                        0,
                        start,
                        start
                    ),
                    (
                        "ValueError: bad id <n>".to_owned(),
                        "open".to_owned(),
                        0,
                        start,
                        later
                    ),
                ]
            );
        }

        #[tokio::test]
        async fn filter_limits_the_spans() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, Some("service_name = 'api'")).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(trace_id(1), start, "api", "ValueError", "bad id 42"),
                    error_span(trace_id(2), start, "worker", "KeyError", "missing key"),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let titles: Vec<String> = issues_of(&app).await.into_iter().map(|row| row.0).collect();
            assert_eq!(titles, vec!["ValueError: bad id <n>".to_owned()]);
        }

        #[tokio::test]
        async fn a_bad_stored_filter_skips_the_source_only() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            let broken = error_source(&app, project, None).await;
            error_source(&app, project, None).await;
            exec(
                &app,
                "UPDATE issue_sources SET filter = ? WHERE id = ?",
                vec!["((".to_owned(), broken.to_string()],
            )
            .await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            // Only the source with a working filter created the issue.
            assert_eq!(issues_of(&app).await.len(), 1);
        }

        #[tokio::test]
        async fn rules_and_sources_share_a_batch() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            rule(&app, project, "trace", None, 1.0).await;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let report = sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(report.enqueued, 1);
            assert_eq!(jobs(&app).await.len(), 1);
            assert_eq!(issues_of(&app).await.len(), 1);
        }

        #[tokio::test]
        async fn eval_traces_are_never_issues() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let mut eval_span = error_span(trace_id(9), start, "api", "ValueError", "bad id 42");
            eval_span.environment = Some(cetra_core::names::environment::EVAL.to_owned());
            app.commit_spans(&seeded.project, vec![eval_span]).await;
            // Evaluation spans are not recorded as pending, so the trace is forced into the map.
            app.state.pending.load(vec![(
                project,
                trace_id(9),
                Entry {
                    first_seen: at,
                    last_seen: at,
                    min_start: start,
                    root_ended: true,
                    failed_reads: 0,
                    spans: 1,
                },
            )]);
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert!(issues_of(&app).await.is_empty());
        }

        #[tokio::test]
        async fn a_failed_error_read_retries_the_batch() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            faults::fail_next_read(RunError::Timeout);
            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.failed_batches, 1);
            assert!(issues_of(&app).await.is_empty());
            assert_eq!(app.state.pending.len(), 1, "the trace stays pending");

            let report = sweep_once(&app.state, now).await.unwrap();
            assert_eq!(report.failed_batches, 0);
            assert_eq!(issues_of(&app).await.len(), 1);
            assert_eq!(app.state.pending.len(), 0);
        }

        /// The live occurrence scores of `trace` in `project`, as the API lists them.
        async fn occurrences_on(
            app: &TestApp,
            seeded: &Seeded,
            project: ProjectId,
            trace: TraceId,
        ) -> Vec<cetra_core::score::Score> {
            let response = app
                .request(
                    Method::GET,
                    &format!("/api/v1/projects/{project}/scores?trace_id={trace}&source=issue"),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .send()
                .await;
            let page: Value = response.json();
            page["data"]
                .as_array()
                .expect("data")
                .iter()
                .map(|item| serde_json::from_value(item.clone()).expect("a score"))
                .collect()
        }

        /// The id of the issue titled `title`.
        async fn issue_id_of(app: &TestApp, title: &str) -> Uuid {
            let (id,): (String,) = sqlx::query_as("SELECT id FROM issues WHERE title = ?")
                .bind(title)
                .fetch_one(app.state.meta.reader())
                .await
                .unwrap();
            Uuid::parse_str(&id).unwrap()
        }

        #[tokio::test]
        async fn error_span_becomes_an_occurrence() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            let mut span = error_span(trace_id(1), start, "api", "ValueError", "bad id 42");
            span.user_id = Some("user-1".to_owned());
            span.session_id = Some("session-1".to_owned());
            app.commit_spans(&seeded.project, vec![span]).await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let issue = issue_id_of(&app, "ValueError: bad id <n>").await;
            let scores = occurrences_on(&app, &seeded, project, trace_id(1)).await;
            assert_eq!(scores.len(), 1, "{scores:?}");
            let score = &scores[0];
            assert_eq!(score.name, format!("issue:{issue}"));
            assert_eq!(score.source, cetra_core::enums::ScoreSource::Issue);
            assert_eq!(score.created_at, (start / 1_000_000) * 1_000_000);
            assert_eq!(score.user_id.as_deref(), Some("user-1"));
            assert_eq!(score.session_id.as_deref(), Some("session-1"));
            assert_eq!(score.metadata.len(), 2, "{:?}", score.metadata);
            assert!(score.metadata.contains_key("cetra.issue.span_id"));
        }

        #[tokio::test]
        async fn two_error_spans_of_one_issue_in_one_trace_give_one_occurrence() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            let start = at - 30 * SECOND;
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(
                        trace_id(1),
                        start + SECOND,
                        "api",
                        "ValueError",
                        "bad id 42",
                    ),
                    error_span(trace_id(1), start, "api", "ValueError", "bad id 43"),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let scores = occurrences_on(&app, &seeded, project, trace_id(1)).await;
            assert_eq!(scores.len(), 1, "{scores:?}");
            // The occurrence is the trace's earliest error span.
            assert_eq!(scores[0].created_at, (start / 1_000_000) * 1_000_000);
        }

        #[tokio::test]
        async fn a_resweep_writes_no_second_occurrence() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            // A later span of the same error joins the trace; its earliest span is unchanged.
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 20 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 43",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            let scores = occurrences_on(&app, &seeded, project, trace_id(1)).await;
            assert_eq!(scores.len(), 1, "{scores:?}");
            assert_eq!(
                scores[0].created_at,
                ((at - 30 * SECOND) / 1_000_000) * 1_000_000
            );
        }

        #[tokio::test]
        async fn a_future_span_start_is_clamped_to_now() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at + 10 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            sweep_once(&app.state, now).await.unwrap();
            let scores = occurrences_on(&app, &seeded, project, trace_id(1)).await;
            assert_eq!(scores.len(), 1, "{scores:?}");
            assert_eq!(scores[0].created_at, (now / 1_000_000) * 1_000_000);
        }

        #[tokio::test]
        async fn an_occurrence_older_than_retention_is_skipped() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let (short, _) = app.add_project(&seeded, "short", Some(1)).await;
            error_source(&app, short.id, None).await;
            let at = set_clock(&app);
            let day = 86_400 * SECOND;
            app.commit_spans(
                &short,
                vec![
                    error_span(trace_id(1), at - 2 * day, "api", "ValueError", "bad id 1"),
                    error_span(
                        trace_id(2),
                        at - 30 * SECOND,
                        "api",
                        "ValueError",
                        "bad id 2",
                    ),
                ],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert!(
                occurrences_on(&app, &seeded, short.id, trace_id(1))
                    .await
                    .is_empty()
            );
            // The rest of the batch is still written.
            assert_eq!(
                occurrences_on(&app, &seeded, short.id, trace_id(2))
                    .await
                    .len(),
                1
            );
        }

        #[tokio::test]
        async fn ignored_issue_still_gets_occurrences() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            exec(
                &app,
                "UPDATE issues SET state = 'ignored' WHERE title = ?",
                vec!["ValueError: bad id <n>".to_owned()],
            )
            .await;
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(2),
                    at - 20 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 43",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert_eq!(issues_of(&app).await[0].1, "ignored");
            assert_eq!(
                occurrences_on(&app, &seeded, project, trace_id(2))
                    .await
                    .len(),
                1
            );
        }

        #[tokio::test]
        async fn occurrences_bill_no_units() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![error_span(
                    trace_id(1),
                    at - 30 * SECOND,
                    "api",
                    "ValueError",
                    "bad id 42",
                )],
            )
            .await;
            app.state.clock.advance(Duration::from_secs(6));
            let now = app.state.clock.now_ns();
            let before = app
                .state
                .usage
                .month_units(&app.state.meta, seeded.org_id, now)
                .await
                .unwrap();
            sweep_once(&app.state, now).await.unwrap();
            assert_eq!(
                occurrences_on(&app, &seeded, project, trace_id(1))
                    .await
                    .len(),
                1
            );
            let after = app
                .state
                .usage
                .month_units(&app.state.meta, seeded.org_id, now)
                .await
                .unwrap();
            assert_eq!(after, before);
        }

        #[tokio::test]
        async fn deletion_guard_blocks_occurrences() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            error_source(&app, project, None).await;
            let at = set_clock(&app);
            app.commit_spans(
                &seeded.project,
                vec![
                    error_span(
                        trace_id(1),
                        at - 30 * SECOND,
                        "api",
                        "ValueError",
                        "bad id 42",
                    ),
                    error_span(
                        trace_id(2),
                        at - 30 * SECOND,
                        "api",
                        "ValueError",
                        "bad id 43",
                    ),
                ],
            )
            .await;
            let now = app.state.clock.now_ns();
            app.state
                .deletions
                .guard
                .add_traces(project, &[trace_id(1)], Uuid::now_v7(), now);
            app.state.clock.advance(Duration::from_secs(6));
            sweep_once(&app.state, app.state.clock.now_ns())
                .await
                .unwrap();
            assert!(
                occurrences_on(&app, &seeded, project, trace_id(1))
                    .await
                    .is_empty()
            );
            assert_eq!(
                occurrences_on(&app, &seeded, project, trace_id(2))
                    .await
                    .len(),
                1
            );
            // The issue itself is still created: only the occurrence is dropped.
            assert_eq!(issues_of(&app).await.len(), 1);
        }
    }
}
