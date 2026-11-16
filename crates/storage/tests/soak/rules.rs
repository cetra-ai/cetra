//! The pass/fail rules of a soak (track R M7, R7.6). Pure functions over the
//! samples, the failure counts and the run config.

use std::collections::BTreeMap;

use super::config::{QueryKind, SoakConfig};
use super::failures::FailureKind;
use super::sample::Sample;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;
/// A window needs this many samples before its 95th percentile means anything.
const GROWTH_MIN_SAMPLES: usize = 8;
/// Longest a deletion may stay pending: `deletion.max_delay` (spec 03 §7).
const DELETION_MAX_DELAY_S: f64 = 24.0 * 3600.0;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) enum Verdict {
    Pass,
    Fail(String),
    /// Not enough data to decide: listed in summary.json, does not fail the run.
    Skipped(String),
    /// Computed and reported, but the run is too short for the rule to be meaningful; does not fail the run.
    Info(String),
}

/// Limits read from the storage config, for the rules that bound a size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) wal_segment_bytes: f64,
    pub(crate) flush_max_bytes: f64,
    pub(crate) l0_files: f64,
}

/// What the soak's end of run found: the settled-state conditions that failed,
/// the deletions still pending, and the ERROR lines of the events log.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct EndChecks {
    /// Conditions of the settled state (step 5 of the end of a run) that failed.
    pub(crate) settled: Vec<String>,
    pub(crate) pending_deletions: u64,
    pub(crate) error_lines: u64,
}

fn p95(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * 0.95).ceil() as usize).clamp(1, values.len()) - 1;
    values[index]
}

/// The growth rule: split the series into 4 windows of equal duration and take
/// each one's 95th percentile. It grows when the four strictly increase and the
/// last exceeds the first by more than `rel × first + abs`.
pub(crate) fn grows(series: &[(f64, f64)], rel: f64, abs: f64) -> Verdict {
    let (Some(first), Some(last)) = (series.first(), series.last()) else {
        return Verdict::Skipped("no samples after the warm-up".to_owned());
    };
    let span = last.0 - first.0;
    if span <= 0.0 {
        return Verdict::Skipped("the series covers no time".to_owned());
    }
    let mut windows: Vec<Vec<f64>> = vec![Vec::new(); 4];
    for (t, value) in series {
        let index = (((t - first.0) / span) * 4.0).floor().clamp(0.0, 3.0) as usize;
        windows[index].push(*value);
    }
    if windows.iter().any(|w| w.len() < GROWTH_MIN_SAMPLES) {
        return Verdict::Skipped(format!(
            "fewer than {GROWTH_MIN_SAMPLES} samples in a window"
        ));
    }
    let q: Vec<f64> = windows.iter_mut().map(|w| p95(w)).collect();
    let grows = q[0] < q[1] && q[1] < q[2] && q[2] < q[3] && q[3] - q[0] > rel * q[0] + abs;
    if grows {
        Verdict::Fail(format!("95th percentiles per quarter grow: {q:.3?}"))
    } else {
        Verdict::Pass
    }
}

/// Ingest must keep up: in each window, Δappended / Δtarget of first-group
/// spans is at least 0.99. Windows are 1 h, starting 15 min in, for runs of at
/// least 2 h, and 10 min otherwise. A trailing window under half a window is ignored.
fn ingest_rate(samples: &[Sample], config: &SoakConfig) -> Verdict {
    let window = if config.hours >= 2.0 { 3600.0 } else { 600.0 };
    let total = config.hours * 3600.0;
    let mut start = 900.0;
    let mut checked = 0;
    let mut failures = Vec::new();
    while start < total {
        let end = (start + window).min(total);
        if end - start < window / 2.0 {
            break;
        }
        let first = samples.iter().find(|s| s.elapsed_s >= start);
        let last = samples.iter().rev().find(|s| s.elapsed_s <= end);
        if let (Some(a), Some(b)) = (first, last) {
            let delta_target = b.target_spans_total as f64 - a.target_spans_total as f64;
            if b.elapsed_s > a.elapsed_s && delta_target > 0.0 {
                checked += 1;
                let ratio =
                    (b.appended_spans_total as f64 - a.appended_spans_total as f64) / delta_target;
                if ratio < 0.99 {
                    failures.push(format!("{start:.0}s–{end:.0}s: {ratio:.4}"));
                }
            }
        }
        start += window;
    }
    if checked == 0 {
        Verdict::Skipped("no complete ingest window".to_owned())
    } else if failures.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "ingest below 0.99 of target in windows: {}",
            failures.join(", ")
        ))
    }
}

/// One growth rule over the samples `post`. `value` returns `None` for a
/// sample that has nothing to say about the series (for example a query kind
/// with no queries in that interval).
fn growth(
    name: &str,
    post: &[&Sample],
    enforced: bool,
    rel: f64,
    abs: f64,
    value: impl Fn(&Sample) -> Option<f64>,
) -> (String, Verdict) {
    let series: Vec<(f64, f64)> = post
        .iter()
        .filter_map(|s| value(s).map(|v| (s.elapsed_s, v)))
        .collect();
    let verdict = match grows(&series, rel, abs) {
        verdict @ Verdict::Skipped(_) => verdict,
        verdict if enforced => verdict,
        Verdict::Fail(detail) => Verdict::Info(format!("not enforced in this run: {detail}")),
        _ => Verdict::Info("not enforced in this run: no growth".to_owned()),
    };
    (name.to_owned(), verdict)
}

fn harness(name: &str, failed: Option<String>) -> (String, Verdict) {
    (name.to_owned(), failed.map_or(Verdict::Pass, Verdict::Fail))
}

/// Every rule of R7.6's table, in order: the growth rules first, then the harness rules.
///
/// Samples after the run's end (`hours` plus one sample interval) are the stop and
/// the settle; they are not growth, lag or RSS evidence. Enforced growth reads from
/// `max(warmup_hours, (retention_days + 1) × 24)`, informational growth from
/// `warmup_hours`. `compaction_lag` always reads from `warmup_hours`.
pub(crate) fn evaluate(
    samples: &[Sample],
    failures: &BTreeMap<FailureKind, u64>,
    config: &SoakConfig,
    limits: &Limits,
    extra: &EndChecks,
) -> Vec<(String, Verdict)> {
    let growth_warmup_h = config
        .warmup_hours
        .max(f64::from(config.retention_days + 1) * 24.0);
    let enforced = config.hours >= growth_warmup_h + 24.0;
    let run_end = config.hours * 3600.0 + config.sample_secs as f64;
    let during: Vec<&Sample> = samples.iter().filter(|s| s.elapsed_s <= run_end).collect();
    let from_h = if enforced {
        growth_warmup_h
    } else {
        config.warmup_hours
    };
    let post: Vec<&Sample> = during
        .iter()
        .copied()
        .filter(|s| s.elapsed_s >= from_h * 3600.0)
        .collect();
    let after_warmup: Vec<&Sample> = during
        .iter()
        .copied()
        .filter(|s| s.elapsed_s >= config.warmup_hours * 3600.0)
        .collect();
    let count = |kind: FailureKind| failures.get(&kind).copied().unwrap_or(0);
    let mut rules = vec![
        growth("rss_growth", &post, enforced, 0.10, 256.0 * MIB, |s| {
            Some(s.rss_bytes as f64)
        }),
        growth("fd_growth", &post, enforced, 0.10, 64.0, |s| {
            Some(s.fds as f64)
        }),
        growth("thread_growth", &post, enforced, 0.10, 8.0, |s| {
            Some(s.threads as f64)
        }),
        growth(
            "wal_growth",
            &post,
            enforced,
            0.5,
            2.0 * limits.wal_segment_bytes,
            |s| Some(s.wal_bytes as f64),
        ),
        growth(
            "memtable_growth",
            &post,
            enforced,
            0.5,
            limits.flush_max_bytes,
            |s| Some(s.memtable_bytes as f64),
        ),
        growth("l0_growth", &post, enforced, 0.5, limits.l0_files, |s| {
            Some(s.files.get("spans").map_or(0.0, |f| f.l0 as f64))
        }),
    ];
    for kind in QueryKind::ALL {
        rules.push(growth(
            &format!("query_growth_{}", kind.name()),
            &post,
            enforced,
            0.5,
            50.0,
            |s| {
                s.queries
                    .get(kind.name())
                    .filter(|q| q.n > 0)
                    .map(|q| q.p95_ms)
            },
        ));
    }

    let max_rss = during
        .iter()
        .map(|s| s.rss_bytes as f64)
        .fold(0.0, f64::max);
    rules.push(harness(
        "rss_max",
        (max_rss > config.max_rss_gib * GIB).then(|| {
            format!(
                "peak RSS {:.2} GiB above {} GiB",
                max_rss / GIB,
                config.max_rss_gib
            )
        }),
    ));
    rules.push(("ingest_rate".to_owned(), ingest_rate(samples, config)));
    let max_lag = after_warmup
        .iter()
        .map(|s| s.l0_oldest_age_s)
        .fold(0.0, f64::max);
    rules.push(harness(
        "compaction_lag",
        (max_lag > (config.max_l0_age_min * 60) as f64).then(|| {
            format!(
                "oldest L0 file {:.0} s old, above {} min",
                max_lag, config.max_l0_age_min
            )
        }),
    ));
    rules.push(harness(
        "day_close",
        samples
            .iter()
            .any(|s| s.overdue_partitions > 0)
            .then(|| "a closed day was not compacted in time".to_owned()),
    ));
    rules.push(harness(
        "retention",
        samples
            .iter()
            .any(|s| s.retention_violations > 0)
            .then(|| "active files older than their retention watermark".to_owned()),
    ));
    let max_pending = samples
        .iter()
        .map(|s| s.oldest_pending_deletion_s)
        .fold(0.0, f64::max);
    rules.push(harness(
        "deletion_lag",
        (max_pending > DELETION_MAX_DELAY_S || extra.pending_deletions > 0).then(|| {
            format!(
                "oldest pending deletion {max_pending:.0} s, {} still pending at the end",
                extra.pending_deletions
            )
        }),
    ));
    rules.push(harness(
        "orphans",
        samples
            .iter()
            .any(|s| s.orphans > 0)
            .then(|| "confirmed orphan objects".to_owned()),
    ));
    rules.push(harness(
        "wal_failed",
        samples
            .iter()
            .any(|s| s.wal_failed)
            .then(|| "a WAL writer stopped".to_owned()),
    ));
    rules.push(harness(
        "sampling",
        (count(FailureKind::Sample) > 0)
            .then(|| format!("{} samples could not be taken", count(FailureKind::Sample))),
    ));
    rules.push(harness(
        "appends",
        (count(FailureKind::Append) > 0)
            .then(|| format!("{} append failures", count(FailureKind::Append))),
    ));
    rules.push(harness(
        "queries",
        (count(FailureKind::Query) > 0)
            .then(|| format!("{} query failures", count(FailureKind::Query))),
    ));
    let deletion_failures = count(FailureKind::Deletion) + count(FailureKind::DeletionVisible);
    rules.push(harness(
        "deletions",
        (deletion_failures > 0).then(|| format!("{deletion_failures} deletion failures")),
    ));
    rules.push(harness(
        "retention_calls",
        (count(FailureKind::Retention) > 0)
            .then(|| format!("{} retention failures", count(FailureKind::Retention))),
    ));
    rules.push(harness(
        "log_errors",
        (extra.error_lines > 0)
            .then(|| format!("{} ERROR lines in the events log", extra.error_lines)),
    ));
    rules.push(harness(
        "oracle",
        (count(FailureKind::Oracle) > 0)
            .then(|| format!("{} oracle mismatches", count(FailureKind::Oracle))),
    ));
    rules.push(harness(
        "settled",
        (!extra.settled.is_empty()).then(|| extra.settled.join("; ")),
    ));
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_is_nearest_rank() {
        let mut values: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(p95(&mut values), 19.0);
    }
}
