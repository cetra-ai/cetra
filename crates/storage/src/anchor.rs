//! Trace anchor: keep the spans and partials of a trace that crosses midnight
//! in one partition day (D86).
//!
//! All anchor logic lives here. The flush asks for one partition day per trace
//! and applies the returned plan only after its catalog commit succeeds. At
//! open, the state is rebuilt from recent `traces` files.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use cetra_column::{Batch, Column, ColumnError, DataType, FlatData, ScalarValue, Selection};
use cetra_core::ids::{ProjectId, TraceId};
use cetra_core::schema::{spans, traces};
use chrono::{Datelike, NaiveDate};
use object_store::ObjectStore;

use crate::catalog::{Catalog, FileMeta, FileTable, now_ns};
use crate::error::AnchorError;
use crate::memtable::{FrozenView, PartitionKey};
use crate::objects::Objects;
use crate::verify::VerifyQueue;

/// Nanoseconds in a UTC day.
pub(crate) const DAY_NS: i64 = 86_400_000_000_000;

/// `NaiveDate::num_days_from_ce` of 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i64 = 719_163;

/// One remembered anchor. `expires` is an ingest-time deadline, not an event age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    day: NaiveDate,
    expires: i64,
}

/// Entry changes planned by one flush, applied only after its commit succeeds.
pub(crate) struct AnchorPlan {
    changes: Vec<(ProjectId, TraceId, Entry)>,
    max_ingest: i64,
}

/// One output partition and the exact rows assigned to it.
pub(crate) struct AnchorPartition {
    pub(crate) key: PartitionKey,
    pub(crate) batch: Batch,
}

/// The anchor state of the spans flush task.
#[derive(Debug)]
pub(crate) struct Anchors {
    /// Midnight window, nanoseconds.
    window: i64,
    /// Largest `ingest_time` committed by any flush, or `i64::MIN`.
    clock: i64,
    entries: HashMap<(ProjectId, TraceId), Entry>,
}

impl Anchors {
    pub(crate) fn new(window: i64) -> Self {
        Self {
            window,
            clock: i64::MIN,
            entries: HashMap::new(),
        }
    }

    /// Regroups a frozen spans view by anchor day and plans the entry changes.
    ///
    /// The state is only read here; [`Self::commit`] applies the plan after the
    /// catalog commit. A failed attempt therefore recomputes the same days.
    pub(crate) fn partition(
        &self,
        view: &FrozenView,
    ) -> Result<(Vec<AnchorPartition>, AnchorPlan), AnchorError> {
        let mut by_project: BTreeMap<ProjectId, ProjectBatches> = BTreeMap::new();
        for (key, batches) in view.partitions.iter() {
            by_project.entry(key.project).or_default().push((
                key.day,
                batches
                    .iter()
                    .map(|batch| Arc::clone(&batch.batch))
                    .collect(),
            ));
        }

        let mut max_ingest = i64::MIN;
        for groups in by_project.values() {
            for (_, batches) in groups {
                for batch in batches {
                    let ingest = ts_column(batch, spans::INGEST_TIME)?;
                    for_each_active_row(batch, |row| {
                        if let Some(value) = read_ts(ingest, row) {
                            max_ingest = max_ingest.max(value);
                        }
                    });
                }
            }
        }
        let expires = max_ingest.saturating_add(self.window.saturating_mul(2));

        let schema = FileTable::Spans.column_schema();
        let mut partitions = Vec::new();
        let mut changes = Vec::new();
        for (project, groups) in &by_project {
            let refs: Vec<&Batch> = groups
                .iter()
                .flat_map(|(_, batches)| batches.iter().map(AsRef::as_ref))
                .collect();
            if refs.is_empty() {
                continue;
            }
            let combined = Batch::concat(Arc::clone(&schema), &refs, None)
                .map_err(AnchorError::from_column)?;
            let trace = fixed16_column(&combined, spans::TRACE_ID)?;
            let start = ts_column(&combined, spans::START_TIME)?;

            let mut by_trace: BTreeMap<[u8; 16], Vec<u32>> = BTreeMap::new();
            for row in 0..combined.num_rows() {
                let Some(trace_bytes) = read_fixed16(trace, row) else {
                    return Err(AnchorError::column(spans::TRACE_ID));
                };
                if read_ts(start, row).is_none() {
                    return Err(AnchorError::column(spans::START_TIME));
                }
                let index = u32::try_from(row).map_err(|_| {
                    AnchorError::from_column(ColumnError::Overflow {
                        reason: "anchor: too many rows in one flush",
                    })
                })?;
                by_trace.entry(trace_bytes).or_default().push(index);
            }

            let mut assigned: BTreeMap<NaiveDate, Vec<u32>> = BTreeMap::new();
            for (trace_bytes, rows) in by_trace {
                let trace_id = TraceId::from_slice(&trace_bytes)
                    .map_err(|_| AnchorError::column(spans::TRACE_ID))?;
                let min_start = rows
                    .iter()
                    .filter_map(|row| read_ts(start, *row as usize))
                    .min()
                    .unwrap_or(0);
                let existing = self.entries.get(&(*project, trace_id)).map(|e| e.day);
                let anchor = existing.unwrap_or_else(|| day_of_ns(min_start));
                let anchor_number = day_number(anchor);
                let mut near = false;
                for row in &rows {
                    let Some(time) = read_ts(start, *row as usize) else {
                        continue;
                    };
                    near |= near_midnight(time, self.window);
                    let own = day_of_ns(time);
                    let day = if (day_number(own) - anchor_number).abs() <= 1 {
                        anchor
                    } else {
                        own
                    };
                    assigned.entry(day).or_default().push(*row);
                }
                // Spec 03 § Trace anchor: a near-midnight row makes an entry,
                // and an existing entry is refreshed by the same deadline even
                // when none of this flush's rows is near midnight.
                if near || existing.is_some() {
                    changes.push((
                        *project,
                        trace_id,
                        Entry {
                            day: anchor,
                            expires,
                        },
                    ));
                }
            }

            for (day, mut indices) in assigned {
                indices.sort_unstable();
                let batch = combined
                    .with_selection(Selection::Indices(indices))
                    .and_then(|selected| selected.compact(None))
                    .map_err(AnchorError::from_column)?;
                partitions.push(AnchorPartition {
                    key: PartitionKey {
                        project: *project,
                        day,
                    },
                    batch,
                });
            }
        }
        partitions.sort_by_key(|partition| partition.key);
        Ok((
            partitions,
            AnchorPlan {
                changes,
                max_ingest,
            },
        ))
    }

    /// Applies a plan after a successful commit: insert or refresh entries,
    /// advance the clock, then drop entries that expired before it.
    pub(crate) fn commit(&mut self, plan: AnchorPlan) {
        for (project, trace, entry) in plan.changes {
            self.entries.insert((project, trace), entry);
        }
        self.clock = self.clock.max(plan.max_ingest);
        self.entries.retain(|_, entry| entry.expires >= self.clock);
    }

    /// Rebuilds the state from recent `traces` files, before the flush task starts.
    ///
    /// A traces file that cannot be read is skipped and becomes a suspect (spec
    /// 03 §10). A store failure still fails the open, as before.
    pub(crate) async fn rebuild(
        &mut self,
        catalog: &Catalog,
        objects: &Objects,
        verify: &VerifyQueue,
    ) -> Result<(), AnchorError> {
        let started = Instant::now();
        self.clock = catalog
            .files_all(FileTable::Spans)
            .map(|file| file.max_ingest_time)
            .max()
            .unwrap_or(i64::MIN);
        let since = self.clock.saturating_sub(self.window.saturating_mul(2));
        let files: Vec<&Arc<FileMeta>> = catalog
            .files_all(FileTable::Traces)
            .filter(|file| file.max_ingest_time >= since)
            .collect();
        let mut rows_read = 0_usize;
        for file in &files {
            match self.rebuild_file(file, objects.store(), since).await {
                Ok(rows) => rows_read = rows_read.saturating_add(rows),
                Err(error) if error.is_store_error() => {
                    return Err(error);
                }
                Err(_) => {
                    // The anchors collected from this file stay: they are hints.
                    tracing::warn!(
                        file_id = %file.id,
                        "skipping an unreadable traces file in the anchor rebuild"
                    );
                    verify.suspect([file.id], now_ns());
                }
            }
        }
        tracing::info!(
            files = files.len(),
            rows = rows_read,
            entries = self.entries.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "rebuilt trace anchors"
        );
        Ok(())
    }

    /// Reads the recent row groups of one `traces` file; returns the rows read.
    pub(crate) async fn rebuild_file(
        &mut self,
        file: &FileMeta,
        store: Arc<dyn ObjectStore>,
        since: i64,
    ) -> Result<usize, AnchorError> {
        let batches = crate::file_format::read_trace_anchors(&store, file, since).await?;
        let mut rows = 0_usize;
        for batch in &batches {
            rows = rows.saturating_add(batch.num_rows());
            collect_rebuild_rows(self, file, batch, since)?;
        }
        Ok(rows)
    }
}

fn collect_rebuild_rows(
    anchors: &mut Anchors,
    file: &FileMeta,
    batch: &Batch,
    since: i64,
) -> Result<(), AnchorError> {
    let trace = fixed16_column(batch, traces::TRACE_ID)?;
    let start = ts_column(batch, traces::START_TIME)?;
    let end = ts_column(batch, traces::END_TIME)?;
    let ingest = ts_column(batch, traces::LAST_INGEST_TIME)?;
    for_each_active_row(batch, |row| {
        let Some(trace_bytes) = read_fixed16(trace, row) else {
            return;
        };
        let Some(start_ns) = read_ts(start, row) else {
            return;
        };
        let Some(last_ingest) = read_ts(ingest, row) else {
            return;
        };
        // A partial's start_time is the trace's earliest start; a child ending
        // near midnight is only visible through end_time. Without it a trace
        // rooted well before midnight loses its anchor on restart.
        let near = near_midnight(start_ns, anchors.window)
            || read_ts(end, row).is_some_and(|e| near_midnight(e, anchors.window));
        if last_ingest < since || !near {
            return;
        }
        let Ok(trace_id) = TraceId::from_slice(&trace_bytes) else {
            return;
        };
        let expires = last_ingest.saturating_add(anchors.window.saturating_mul(2));
        match anchors.entries.get_mut(&(file.project, trace_id)) {
            Some(entry) => {
                if file.day < entry.day {
                    entry.day = file.day;
                }
                entry.expires = entry.expires.max(expires);
            }
            None => {
                anchors.entries.insert(
                    (file.project, trace_id),
                    Entry {
                        day: file.day,
                        expires,
                    },
                );
            }
        }
    });
    Ok(())
}

fn ts_column<'a>(batch: &'a Batch, name: &'static str) -> Result<&'a Column, AnchorError> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| AnchorError::column(name))?;
    if !matches!(col.data_type(), DataType::TimestampNs { .. }) {
        return Err(AnchorError::column(name));
    }
    Ok(col)
}

fn fixed16_column<'a>(batch: &'a Batch, name: &'static str) -> Result<&'a Column, AnchorError> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| AnchorError::column(name))?;
    if col.data_type() != DataType::FixedBytes16 {
        return Err(AnchorError::column(name));
    }
    Ok(col)
}

fn read_ts(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::TimestampNs { values, .. } => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

fn read_fixed16(col: &Column, row: usize) -> Option<[u8; 16]> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::FixedBytes16(values) => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::FixedBytes16(bytes) => Some(*bytes),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::FixedBytes16(values) => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::FixedBytes16(values) => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

fn for_each_active_row(batch: &Batch, mut f: impl FnMut(usize)) {
    match batch.selection() {
        Selection::All => {
            for row in 0..batch.raw_len() {
                f(row);
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                f(row);
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                f(row as usize);
            }
        }
    }
}

/// Batches of one project, grouped by their own UTC day.
type ProjectBatches = Vec<(NaiveDate, Vec<Arc<Batch>>)>;

/// The midnight closest to `t`; exactly noon rounds up.
fn nearest_midnight(t: i64) -> i64 {
    let days = t.div_euclid(DAY_NS);
    let rest = t.rem_euclid(DAY_NS);
    let days = if rest >= DAY_NS / 2 {
        days.saturating_add(1)
    } else {
        days
    };
    days.saturating_mul(DAY_NS)
}

/// Strictly inside `window` of the nearest midnight.
fn near_midnight(t: i64, window: i64) -> bool {
    t.abs_diff(nearest_midnight(t)) < window as u64
}

fn day_number(day: NaiveDate) -> i64 {
    i64::from(day.num_days_from_ce()) - UNIX_EPOCH_DAYS_FROM_CE
}

fn day_of_ns(ns: i64) -> NaiveDate {
    let days = ns.div_euclid(DAY_NS);
    let ce = days.saturating_add(UNIX_EPOCH_DAYS_FROM_CE);
    i32::try_from(ce)
        .ok()
        .and_then(NaiveDate::from_num_days_from_ce_opt)
        .unwrap_or(NaiveDate::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use cetra_core::batch::spans_to_batch;
    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::span::Span;
    use chrono::{Duration as ChronoDuration, NaiveDate};

    use super::*;
    use crate::memtable::MemBatch;

    const NS_PER_SEC: i64 = 1_000_000_000;
    const HOUR_NS: i64 = 3_600 * NS_PER_SEC;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, 5).unwrap()
    }

    fn at(day: NaiveDate, hour: i64, minute: i64, second: i64) -> i64 {
        day_number(day) * DAY_NS + (hour * 3_600 + minute * 60 + second) * NS_PER_SEC
    }

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn trace(last: u8) -> TraceId {
        let mut bytes = [0_u8; 16];
        bytes[15] = last;
        TraceId::from_bytes(bytes).unwrap()
    }

    fn span_id(last: u8) -> SpanId {
        let mut bytes = [0_u8; 8];
        bytes[7] = last;
        SpanId::from_bytes(bytes).unwrap()
    }

    fn span(trace_last: u8, span_last: u8, start: i64, ingest: i64) -> Span {
        Span {
            project_id: project(),
            trace_id: trace(trace_last),
            span_id: span_id(span_last),
            parent_span_id: None,
            name: format!("s{span_last}"),
            span_kind: SpanKind::Span,
            start_time: start,
            end_time: start + 100,
            status: SpanStatus::Ok,
            status_message: None,
            service_name: None,
            session_id: None,
            user_id: None,
            environment: None,
            release: None,
            tags: Vec::new(),
            metadata: BTreeMap::new(),
            provider: None,
            request_model: None,
            response_model: None,
            prompt_name: None,
            prompt_version: None,
            operation: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_input: None,
            cost_output: None,
            cost_total: None,
            cost_source: None,
            ttft_ns: None,
            request_params: None,
            input: None,
            input_format: None,
            output: None,
            output_format: None,
            input_preview: None,
            output_preview: None,
            system_instructions: None,
            tool_definitions: None,
            attributes: BTreeMap::new(),
            resource_attributes: BTreeMap::new(),
            scope_name: None,
            scope_version: None,
            events: Vec::new(),
            links: Vec::new(),
            media_refs: Vec::new(),
            truncated: Vec::new(),
            conventions: Vec::new(),
            ingest_time: ingest,
            schema_version: 1,
        }
    }

    /// Builds a frozen view partitioned by each row's own UTC day.
    fn view(spans: &[Span]) -> FrozenView {
        let batch = spans_to_batch(spans).unwrap();
        let starts = ts_column(&batch, spans::START_TIME).unwrap();
        let mut by_day: BTreeMap<NaiveDate, Vec<u32>> = BTreeMap::new();
        for row in 0..batch.num_rows() {
            let start = read_ts(starts, row).unwrap();
            by_day
                .entry(day_of_ns(start))
                .or_default()
                .push(u32::try_from(row).unwrap());
        }
        let mut partitions = BTreeMap::new();
        for (day, indices) in by_day {
            let sliced = batch
                .with_selection(Selection::Indices(indices))
                .and_then(|b| b.compact(None))
                .unwrap();
            let stored = MemBatch::from_batch(crate::wal::Table::Spans, sliced).unwrap();
            partitions.insert(
                PartitionKey {
                    project: project(),
                    day,
                },
                vec![stored],
            );
        }
        FrozenView {
            partitions: Arc::new(partitions),
            max_seq: 1,
        }
    }

    fn anchors() -> Anchors {
        Anchors::new(HOUR_NS)
    }

    /// One successful flush: partition, then apply the plan.
    fn flush(anchors: &mut Anchors, spans: &[Span]) -> Vec<(NaiveDate, usize)> {
        let view = view(spans);
        let (partitions, plan) = anchors.partition(&view).unwrap();
        let result = partitions
            .iter()
            .map(|partition| (partition.key.day, partition.batch.num_rows()))
            .collect();
        anchors.commit(plan);
        result
    }

    fn days(partitions: &[AnchorPartition]) -> Vec<NaiveDate> {
        partitions
            .iter()
            .map(|partition| partition.key.day)
            .collect()
    }

    #[test]
    fn nearest_midnight_rounds_exactly_noon_up() {
        let d = day();
        let noon = at(d, 12, 0, 0);
        assert_eq!(nearest_midnight(noon), at(d.succ_opt().unwrap(), 0, 0, 0));
        assert_eq!(nearest_midnight(noon - 1), day_number(d) * DAY_NS);
        assert_eq!(nearest_midnight(at(d, 0, 0, 0)), day_number(d) * DAY_NS);
    }

    #[test]
    fn near_midnight_is_strict_inside_the_window() {
        let midnight = day_number(day()) * DAY_NS;
        assert!(near_midnight(midnight - 1, HOUR_NS));
        assert!(near_midnight(midnight + 1, HOUR_NS));
        assert!(!near_midnight(midnight + HOUR_NS, HOUR_NS));
        assert!(!near_midnight(midnight - HOUR_NS, HOUR_NS));
        assert!(!near_midnight(at(day(), 12, 0, 0), HOUR_NS));
    }

    #[test]
    fn crossing_midnight_keeps_both_flushes_in_day_d() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let late = at(day().succ_opt().unwrap(), 0, 0, 10);
        assert_eq!(
            flush(&mut a, &[span(1, 1, early, early + NS_PER_SEC)]),
            vec![(day(), 1)]
        );
        assert_eq!(
            flush(&mut a, &[span(1, 2, late, late + NS_PER_SEC)]),
            vec![(day(), 1)]
        );
    }

    #[test]
    fn one_flush_across_midnight_lands_in_day_d() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let late = at(day().succ_opt().unwrap(), 0, 0, 10);
        assert_eq!(
            flush(
                &mut a,
                &[
                    span(1, 1, early, early + NS_PER_SEC),
                    span(1, 2, late, late + NS_PER_SEC),
                ]
            ),
            vec![(day(), 2)]
        );
    }

    #[test]
    fn a_late_root_follows_the_anchor_to_day_d_plus_one() {
        let mut a = anchors();
        let next = day().succ_opt().unwrap();
        let child = at(next, 0, 0, 20);
        let root = at(day(), 23, 59, 40);
        assert_eq!(
            flush(&mut a, &[span(1, 1, child, child + NS_PER_SEC)]),
            vec![(next, 1)]
        );
        assert_eq!(
            flush(&mut a, &[span(1, 2, root, root + NS_PER_SEC)]),
            vec![(next, 1)]
        );
    }

    #[test]
    fn a_row_two_days_from_the_anchor_stays_in_its_own_day() {
        let mut a = anchors();
        let far = day().succ_opt().unwrap().succ_opt().unwrap();
        let first = at(day(), 12, 0, 0);
        let second = at(far, 12, 0, 0);
        assert_eq!(
            flush(
                &mut a,
                &[
                    span(1, 1, first, first + NS_PER_SEC),
                    span(1, 2, second, second + NS_PER_SEC),
                ]
            ),
            vec![(day(), 1), (far, 1)]
        );
    }

    #[test]
    fn a_late_row_two_days_after_the_anchor_stays_in_its_own_day() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let far = day().succ_opt().unwrap().succ_opt().unwrap();
        let late = at(far, 12, 0, 0);
        assert_eq!(
            flush(&mut a, &[span(1, 1, early, early + NS_PER_SEC)]),
            vec![(day(), 1)]
        );
        assert_eq!(
            flush(&mut a, &[span(1, 2, late, late + NS_PER_SEC)]),
            vec![(far, 1)]
        );
    }

    #[test]
    fn an_expired_entry_no_longer_anchors_a_late_row() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let ingest = early + NS_PER_SEC;
        assert_eq!(
            flush(&mut a, &[span(1, 1, early, ingest)]),
            vec![(day(), 1)]
        );
        // A different trace, ingested past the first entry's expiry, advances
        // the clock and drops it without refreshing it.
        let noon = at(day(), 12, 0, 0);
        let past = ingest + 3 * HOUR_NS;
        flush(&mut a, &[span(2, 2, noon, past)]);
        let late = at(day().succ_opt().unwrap(), 0, 10, 0);
        assert_eq!(
            flush(&mut a, &[span(1, 3, late, late + NS_PER_SEC)]),
            vec![(day().succ_opt().unwrap(), 1)]
        );
    }

    #[test]
    fn backfilled_events_ingested_now_are_anchored_by_their_start_day() {
        let mut a = anchors();
        let old = day() - ChronoDuration::days(30);
        let early = at(old, 23, 59, 50);
        let late = at(old.succ_opt().unwrap(), 0, 0, 10);
        let now = at(day(), 12, 0, 0);
        // Two flushes: the entry made by the first must survive to the second
        // because expiry counts ingest time, not the 30-day-old event time.
        assert_eq!(flush(&mut a, &[span(1, 1, early, now)]), vec![(old, 1)]);
        assert_eq!(
            flush(&mut a, &[span(1, 2, late, now + HOUR_NS)]),
            vec![(old, 1)]
        );
    }

    /// Spec 03 § Trace anchor: an existing entry is refreshed by every flush
    /// with rows for that trace, not only flushes with a near-midnight row.
    #[test]
    fn an_existing_entry_is_refreshed_without_a_midnight_row() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let ingest = early + NS_PER_SEC;
        assert_eq!(
            flush(&mut a, &[span(1, 1, early, ingest)]),
            vec![(day(), 1)]
        );
        // Noon is far from midnight and three ingest-hours later. Without a
        // refresh, the original deadline (ingest + 2h) would pass the clock
        // and the entry would be dropped.
        let noon = at(day().succ_opt().unwrap(), 12, 0, 0);
        let past = ingest + 3 * HOUR_NS;
        assert_eq!(flush(&mut a, &[span(1, 2, noon, past)]), vec![(day(), 1)]);
        // The refreshed anchor still pulls a late row back to day D.
        let late = at(day().succ_opt().unwrap(), 0, 0, 10);
        assert_eq!(
            flush(&mut a, &[span(1, 3, late, past + NS_PER_SEC)]),
            vec![(day(), 1)]
        );
    }

    #[test]
    fn a_failed_attempt_changes_no_state_and_assigns_the_same_days() {
        let mut a = anchors();
        let early = at(day(), 23, 59, 50);
        let view = view(&[span(1, 1, early, early + NS_PER_SEC)]);
        let (first, plan) = a.partition(&view).unwrap();
        let (retry, _) = a.partition(&view).unwrap();
        assert_eq!(days(&first), days(&retry));
        assert!(
            a.entries.is_empty(),
            "an uncommitted attempt wrote an entry"
        );
        a.commit(plan);
        assert_eq!(a.entries.len(), 1);
    }
}
