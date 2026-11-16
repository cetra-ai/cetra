//! The sampled-trace registry and the oracle check (track R M7, R7.6).
//!
//! The registry knows what storage must show for each sampled trace: visible,
//! deleted by a deletion, or retained by retention. `check_sample` reads a
//! seeded sample of them from storage and records every mismatch.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Float64Type, Int64Type};
use cetra_core::ids::{ProjectId, TraceId};
use cetra_core::schema::traces;
use cetra_storage::{Storage, trace_detail};
use chrono::{DateTime, NaiveDate};

use super::driver::context;
use super::failures::{FailureKind, Failures};
use super::sample::now_ns;
use crate::common::payload::TraceSummary;
use crate::common::rng::Rng;

/// A trace's age (since its last ingest) before the oracle may check it.
const SETTLED_NS: i64 = 5 * 60 * 1_000_000_000;

/// What storage must show for a sampled trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    Visible,
    Deleted,
    Retained,
    /// Not checked: a deletion covers it only partly, or it is still ingesting.
    Skip,
}

#[derive(Debug, Clone)]
pub(crate) struct Sampled {
    pub(crate) summary: TraceSummary,
    /// Last ingest time of its spans so far.
    pub(crate) max_ingest: i64,
    /// Every group appended.
    pub(crate) complete: bool,
    pub(crate) state: Expect,
}

/// Shared by the ingest, deletion and retention tasks, behind a std mutex.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    traces: BTreeMap<(usize, TraceId), Sampled>,
}

/// Whether `ns` is within an hour of a UTC midnight. The anchor rule (spec 03)
/// keeps such traces out of the oracle.
pub(crate) fn near_midnight(ns: i64) -> bool {
    const DAY: i64 = 86_400 * 1_000_000_000;
    const HOUR: i64 = 3_600 * 1_000_000_000;
    let of_day = ns.rem_euclid(DAY);
    !(HOUR..=DAY - HOUR).contains(&of_day)
}

fn day_of(ns: i64) -> NaiveDate {
    DateTime::from_timestamp_nanos(ns).date_naive()
}

impl Registry {
    /// Registers a sampled trace when it is generated.
    pub(crate) fn insert(&mut self, project: usize, summary: TraceSummary, complete: bool) {
        let key = (project, summary.trace_id);
        self.traces.insert(
            key,
            Sampled {
                summary,
                max_ingest: 0,
                complete,
                state: Expect::Visible,
            },
        );
    }

    /// The state the registry expects of `trace`, if it is registered.
    pub(crate) fn state(&self, project: usize, trace: TraceId) -> Option<Expect> {
        self.traces.get(&(project, trace)).map(|entry| entry.state)
    }

    /// Records an ingest of `trace` at `ingest`, if the trace is registered.
    pub(crate) fn touch(&mut self, project: usize, trace: TraceId, ingest: i64) {
        if let Some(entry) = self.traces.get_mut(&(project, trace)) {
            entry.max_ingest = entry.max_ingest.max(ingest);
        }
    }

    /// Marks `trace` complete: every group is appended.
    pub(crate) fn complete(&mut self, project: usize, trace: TraceId) {
        if let Some(entry) = self.traces.get_mut(&(project, trace)) {
            entry.complete = true;
        }
    }

    /// The registered traces of one user, for a `User` deletion's id list.
    pub(crate) fn user_trace_ids(&self, project: usize, user: &str) -> Vec<TraceId> {
        self.traces
            .iter()
            .filter(|((p, _), entry)| {
                *p == project && entry.summary.user_id.as_deref() == Some(user)
            })
            .map(|((_, id), _)| *id)
            .collect()
    }

    /// Applies a deletion registered at `cutoff`. A listed, complete trace
    /// whose ingest is at or before the cutoff becomes `Deleted`. A listed
    /// trace that is not complete, and a registered trace of `user` that is
    /// not listed, become `Skip`.
    pub(crate) fn apply_deletion(
        &mut self,
        project: usize,
        trace_ids: &[TraceId],
        user: Option<&str>,
        cutoff: i64,
    ) {
        for ((p, id), entry) in &mut self.traces {
            if *p != project || entry.state != Expect::Visible {
                continue;
            }
            if trace_ids.contains(id) {
                entry.state = if entry.complete && entry.max_ingest <= cutoff {
                    Expect::Deleted
                } else {
                    Expect::Skip
                };
            } else if user.is_some() && entry.summary.user_id.as_deref() == user {
                entry.state = Expect::Skip;
            }
        }
    }

    /// Applies retention with the boundary `keep_from`: visible traces that start before it become `Retained`.
    pub(crate) fn apply_retention(&mut self, project: usize, keep_from: NaiveDate) {
        for ((p, _), entry) in &mut self.traces {
            if *p == project
                && entry.state == Expect::Visible
                && day_of(entry.summary.start_time) < keep_from
            {
                entry.state = Expect::Retained;
            }
        }
    }

    /// The traces the oracle may check: up to `live` visible settled ones, and up
    /// to `gone` deleted and `gone` retained ones, chosen by the seed.
    fn picks(
        &self,
        now: i64,
        live: usize,
        gone: usize,
        seed: u64,
    ) -> Vec<((usize, TraceId), Sampled)> {
        let mut rng = Rng::new(seed | 1);
        let mut visible: Vec<((usize, TraceId), Sampled)> = self
            .traces
            .iter()
            .filter(|(_, e)| {
                e.state == Expect::Visible && e.complete && now - e.max_ingest > SETTLED_NS
            })
            .map(|(k, e)| (*k, e.clone()))
            .collect();
        let mut deleted: Vec<((usize, TraceId), Sampled)> = self
            .traces
            .iter()
            .filter(|(_, e)| e.state == Expect::Deleted)
            .map(|(k, e)| (*k, e.clone()))
            .collect();
        let mut retained: Vec<((usize, TraceId), Sampled)> = self
            .traces
            .iter()
            .filter(|(_, e)| e.state == Expect::Retained)
            .map(|(k, e)| (*k, e.clone()))
            .collect();
        let mut out = Vec::new();
        for (pool, count) in [
            (&mut visible, live),
            (&mut deleted, gone),
            (&mut retained, gone),
        ] {
            shuffle(pool, &mut rng);
            out.extend(pool.iter().take(count).cloned());
        }
        out
    }
}

/// Fisher–Yates shuffle with the seeded generator.
fn shuffle<T>(items: &mut [T], rng: &mut Rng) {
    for i in (1..items.len()).rev() {
        items.swap(i, rng.below(i + 1));
    }
}

/// The first row's value of `name`, cast to `Int64`, or `None` for a null.
fn int_at(batch: &RecordBatch, name: &str) -> Result<Option<i64>, String> {
    let column: ArrayRef = batch
        .column_by_name(name)
        .cloned()
        .ok_or_else(|| format!("no column {name}"))?;
    let cast = cast(&column, &DataType::Int64).map_err(|error| error.to_string())?;
    let array = cast.as_primitive::<Int64Type>();
    Ok((!array.is_null(0)).then(|| array.value(0)))
}

fn float_at(batch: &RecordBatch, name: &str) -> Result<Option<f64>, String> {
    let column: ArrayRef = batch
        .column_by_name(name)
        .cloned()
        .ok_or_else(|| format!("no column {name}"))?;
    let cast = cast(&column, &DataType::Float64).map_err(|error| error.to_string())?;
    let array = cast.as_primitive::<Float64Type>();
    Ok((!array.is_null(0)).then(|| array.value(0)))
}

fn string_at(batch: &RecordBatch, name: &str) -> Result<Option<String>, String> {
    let column: ArrayRef = batch
        .column_by_name(name)
        .cloned()
        .ok_or_else(|| format!("no column {name}"))?;
    let cast = cast(&column, &DataType::Utf8).map_err(|error| error.to_string())?;
    let array = cast.as_string::<i32>();
    Ok((!array.is_null(0)).then(|| array.value(0).to_owned()))
}

/// The mismatches between a stored trace row and what the registry expects.
fn compare(
    hex: &str,
    summary: &TraceSummary,
    detail: &cetra_storage::TraceDetailData,
) -> Vec<String> {
    let mut out = Vec::new();
    let row = &detail.trace;
    if row.num_rows() != 1 {
        out.push(format!(
            "{hex}: trace rows expected 1 got {}",
            row.num_rows()
        ));
        return out;
    }
    let mut check = |field: &str, expected: String, got: Result<String, String>| match got {
        Ok(got) if got == expected => {}
        Ok(got) => out.push(format!("{hex}: {field} expected {expected} got {got}")),
        Err(error) => out.push(format!("{hex}: {field} unreadable: {error}")),
    };
    let text = |value: Result<Option<String>, String>| {
        value.map(|v| v.unwrap_or_else(|| "null".to_owned()))
    };
    let number = |value: Result<Option<i64>, String>| {
        value.map(|v| v.map_or("null".to_owned(), |n| n.to_string()))
    };
    check(
        "name",
        summary.name.clone(),
        text(string_at(row, traces::NAME)),
    );
    check(
        "start_time",
        summary.start_time.to_string(),
        number(int_at(row, traces::START_TIME)),
    );
    check(
        "end_time",
        summary.end_time.to_string(),
        number(int_at(row, traces::END_TIME)),
    );
    check(
        "span_count",
        summary.span_count.to_string(),
        number(int_at(row, traces::SPAN_COUNT)),
    );
    check(
        "error_count",
        summary.error_count.to_string(),
        number(int_at(row, traces::ERROR_COUNT)),
    );
    check(
        "llm_count",
        summary.llm_count.to_string(),
        number(int_at(row, traces::LLM_COUNT)),
    );
    check(
        "total_tokens",
        summary
            .total_tokens
            .map_or("null".to_owned(), |t| t.to_string()),
        number(int_at(row, traces::TOTAL_TOKENS)),
    );
    check(
        "session_id",
        summary
            .session_id
            .clone()
            .unwrap_or_else(|| "null".to_owned()),
        text(string_at(row, traces::SESSION_ID)),
    );
    check(
        "user_id",
        summary.user_id.clone().unwrap_or_else(|| "null".to_owned()),
        text(string_at(row, traces::USER_ID)),
    );
    match (float_at(row, traces::COST_TOTAL), summary.cost_total) {
        (Ok(Some(got)), Some(expected)) if (got - expected).abs() <= 1e-9 => {}
        (Ok(None), None) => {}
        (Ok(got), expected) => out.push(format!(
            "{hex}: cost_total expected {expected:?} got {got:?}"
        )),
        (Err(error), _) => out.push(format!("{hex}: cost_total unreadable: {error}")),
    }
    let stored_spans: usize = detail.spans.iter().map(RecordBatch::num_rows).sum();
    if stored_spans as i64 != summary.span_count {
        out.push(format!(
            "{hex}: span rows expected {} got {stored_spans}",
            summary.span_count
        ));
    }
    out
}

/// One trace's detail, retried once after a second on any error.
async fn detail(
    storage: &Storage,
    project: usize,
    trace: TraceId,
    around: i64,
) -> Result<Option<cetra_storage::TraceDetailData>, String> {
    let hex = trace.to_hex();
    let project_id: ProjectId = super::driver::project_id(project);
    let mut last = Err(String::new());
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let read = match context(storage, project_id, Some(around)).await {
            Ok(ctx) => trace_detail(&ctx, &hex, super::driver::QUERY_TIMEOUT)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        match read {
            Ok(found) => return Ok(found),
            Err(error) => last = Err(error),
        }
    }
    last
}

/// Holds `gate` for its whole run. Compares storage with the registry for up
/// to `live` visible, `gone` deleted and `gone` retained sampled traces (seeded
/// random picks), complete and older than 5 minutes. Returns the mismatches,
/// each "<trace hex>: <field> expected <e> got <g>". A read that fails twice is
/// an `Oracle` failure, not a mismatch.
pub(crate) async fn check_sample(
    storage: &Storage,
    registry: &Mutex<Registry>,
    gate: &tokio::sync::Mutex<()>,
    failures: &Failures,
    live: usize,
    gone: usize,
    seed: u64,
) -> Vec<String> {
    let _gate = gate.lock().await;
    let picks = registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .picks(now_ns(), live, gone, seed);
    let mut mismatches = Vec::new();
    for ((project, trace), sampled) in picks {
        let hex = trace.to_hex();
        let around = sampled.summary.start_time;
        let found = match detail(storage, project, trace, around).await {
            Ok(found) => found,
            Err(error) => {
                failures.record(FailureKind::Oracle, format!("{hex}: read failed: {error}"));
                continue;
            }
        };
        let problems = match (sampled.state, found) {
            (Expect::Visible, None) => vec![format!("{hex}: visible expected, not found")],
            (Expect::Visible, Some(found)) => compare(&hex, &sampled.summary, &found),
            (Expect::Deleted | Expect::Retained, None) => Vec::new(),
            (Expect::Deleted | Expect::Retained, Some(_)) => {
                vec![format!(
                    "{hex}: {:?} expected not found, found",
                    sampled.state
                )]
            }
            (Expect::Skip, _) => Vec::new(),
        };
        for problem in problems {
            failures.record(FailureKind::Oracle, problem.clone());
            mismatches.push(problem);
        }
    }
    mismatches
}
