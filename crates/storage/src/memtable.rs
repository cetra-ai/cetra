//! In-memory batches for one table, partitioned by project and UTC day.
//!
//! Spec 03 §2. In M1 a span's day is the UTC day of its own `start_time`.
//! The trace anchor (spec 03 § Trace anchor) replaces that in M3; this module
//! has no anchor map.
//!
//! Each table has an active memtable and at most one frozen memtable. The
//! writer inserts into the active one. The flush task calls `freeze`.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use cetra_column::{
    Batch, Bitmap, Column, ColumnError, DataType, FlatData, ScalarValue, Selection,
};
use cetra_core::ids::ProjectId;
use chrono::NaiveDate;

use crate::error::MemtableError;
use crate::wal::Table;

/// Nanoseconds in a UTC day. Days are exact; UTC has no leap seconds here.
const DAY_NS: i64 = 86_400_000_000_000;

/// `NaiveDate::num_days_from_ce` of 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i64 = 719_163;

/// A batch smaller than this is eligible for coalescing.
const SMALL_BATCH: usize = 1024 * 1024;

/// Coalesce once a partition holds more small batches than this.
const MAX_SMALL_BATCHES: usize = 64;

/// `(project, day)`, ordered by project then day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PartitionKey {
    /// Project that owns the rows.
    pub project: ProjectId,
    /// UTC day of the table's time column.
    pub day: NaiveDate,
}

/// One stored batch and the accounting a snapshot needs.
#[derive(Debug, Clone)]
pub(crate) struct MemBatch {
    /// Rows as inserted, or a concatenation of small batches.
    pub batch: Arc<Batch>,
    /// Minimum of the table's time column.
    pub min_time: i64,
    /// Maximum of the table's time column.
    pub max_time: i64,
    /// Minimum of the table's `ingest_time` column (spec 03 §7).
    pub min_ingest_time: i64,
    /// [`Batch::memory_bytes`] of `batch`.
    pub bytes: usize,
    /// Trace partials of `batch`, computed once and shared by every read state
    /// that holds this `MemBatch`. A coalesced batch starts with a fresh cache.
    pub partials: Arc<OnceLock<Arc<Batch>>>,
}

/// Rows accepting inserts, and the flush trigger measured from them.
#[derive(Debug, Default)]
struct Active {
    partitions: BTreeMap<PartitionKey, Vec<MemBatch>>,
    bytes: usize,
    /// Highest WAL seq stored here. `0` when nothing has been inserted.
    max_seq: u64,
    /// When the first batch of this memtable was inserted.
    oldest: Option<Instant>,
}

impl Active {
    /// Records `seq` for a batch that was actually stored.
    fn note(&mut self, seq: u64) {
        if self.oldest.is_none() {
            self.oldest = Some(Instant::now());
        }
        self.max_seq = self.max_seq.max(seq);
    }
}

/// One frozen memtable. A flush retries against this until `drop_frozen`.
#[derive(Debug)]
struct Frozen {
    partitions: Arc<BTreeMap<PartitionKey, Vec<MemBatch>>>,
    bytes: usize,
    max_seq: u64,
}

impl Frozen {
    fn from_active(active: Active) -> Self {
        Self {
            partitions: Arc::new(active.partitions),
            bytes: active.bytes,
            max_seq: active.max_seq,
        }
    }

    fn view(&self) -> FrozenView {
        FrozenView {
            partitions: Arc::clone(&self.partitions),
            max_seq: self.max_seq,
        }
    }
}

/// The frozen partitions a flush encodes. A retry receives this same `Arc`.
#[derive(Debug, Clone)]
pub(crate) struct FrozenView {
    /// Batches grouped by partition, in key order.
    pub partitions: Arc<BTreeMap<PartitionKey, Vec<MemBatch>>>,
    /// Highest WAL seq in `partitions`.
    pub max_seq: u64,
}

/// Active rows plus at most one frozen memtable.
#[derive(Debug)]
pub(crate) struct Memtable {
    table: Table,
    active: Active,
    frozen: Option<Frozen>,
}

/// A cheap copy of the batches a query sees.
///
/// Cloning copies the `Arc`s, not the columns. A later coalesce replaces the
/// memtable's `Arc`s and leaves this snapshot alone. `default` has no
/// partitions: the providers of a dropped project read it (spec 03 §7).
#[derive(Debug, Clone, Default)]
pub(crate) struct MemSnapshot {
    /// Batches grouped by partition, in key order.
    pub partitions: BTreeMap<PartitionKey, Vec<MemBatch>>,
}

impl Memtable {
    /// An empty memtable for `table`.
    pub(crate) fn new(table: Table) -> Self {
        Self {
            table,
            active: Active::default(),
            frozen: None,
        }
    }

    /// Bytes of the active and frozen batches, from columnar buffer sizes.
    ///
    /// Backpressure compares this with `memtable.hard_limit`.
    pub(crate) fn bytes(&self) -> usize {
        self.active
            .bytes
            .saturating_add(self.frozen.as_ref().map(|frozen| frozen.bytes).unwrap_or(0))
    }

    /// Bytes of the active memtable. The size flush trigger compares this with
    /// `flush.max_bytes`.
    pub(crate) fn active_bytes(&self) -> usize {
        self.active.bytes
    }

    /// Highest WAL seq in the active memtable, or `0` when it is empty.
    #[cfg(test)]
    pub(crate) fn active_max_seq(&self) -> u64 {
        self.active.max_seq
    }

    /// Highest WAL seq held, active or frozen. `0` when the memtable holds nothing.
    pub(crate) fn max_seq(&self) -> u64 {
        let frozen = self.frozen.as_ref().map_or(0, |frozen| frozen.max_seq);
        self.active.max_seq.max(frozen)
    }

    /// When the first batch of the active memtable was inserted.
    ///
    /// The age flush trigger (M2.6) waits until this plus `flush.max_age`.
    /// Replay counts as the insert.
    pub(crate) fn active_oldest(&self) -> Option<Instant> {
        self.active.oldest
    }

    /// Active partitions and their batches, in key order.
    ///
    /// A [`Self::snapshot`] also includes the frozen batches.
    pub(crate) fn partitions(&self) -> &BTreeMap<PartitionKey, Vec<MemBatch>> {
        &self.active.partitions
    }

    /// A frozen memtable is waiting to be flushed.
    pub(crate) fn has_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// Moves a non-empty active memtable to frozen and returns it.
    ///
    /// A second call, while that frozen memtable is still here, returns the
    /// same view and leaves the active memtable in place. An empty active
    /// memtable with nothing frozen returns `None`.
    pub(crate) fn freeze(&mut self) -> Option<FrozenView> {
        if let Some(frozen) = &self.frozen {
            return Some(frozen.view());
        }
        if self.active.partitions.is_empty() {
            return None;
        }
        let active = std::mem::take(&mut self.active);
        let frozen = Frozen::from_active(active);
        let view = frozen.view();
        self.frozen = Some(frozen);
        Some(view)
    }

    /// Drops the frozen memtable. The active memtable stays.
    pub(crate) fn drop_frozen(&mut self) {
        self.frozen = None;
    }

    /// Appends `batch` under `project`, splitting it when its rows cover more than one day.
    ///
    /// `seq` is the WAL record's sequence number. The day is the UTC day of
    /// [`Table::time_column`]. For spans that is `start_time` in M1; M3
    /// switches spans to the trace anchor. An empty batch changes nothing.
    pub(crate) fn insert(
        &mut self,
        project: ProjectId,
        batch: Batch,
        seq: u64,
    ) -> Result<(), MemtableError> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let time_col_name = self.table.time_column();
        let time_col = ts_column(&batch, time_col_name)?;
        let mut by_day: BTreeMap<NaiveDate, Vec<u32>> = BTreeMap::new();
        let mut single_day: Option<NaiveDate> = None;
        let mut multi_day = false;
        let mut active_days = Vec::with_capacity(batch.num_rows());
        for_each_active_row(&batch, |phys| {
            let Some(ts) = read_ts(time_col, phys) else {
                return Err(MemtableError::BadColumn {
                    column: time_col_name,
                });
            };
            let day = utc_day(ts)?;
            match single_day {
                None => single_day = Some(day),
                Some(first) if first != day => multi_day = true,
                Some(_) => {}
            }
            active_days.push((phys, day));
            Ok(())
        })?;

        if !multi_day {
            if let Some(day) = single_day {
                let stored_batch = if batch.selection().is_all(batch.raw_len()) {
                    batch
                } else {
                    batch.compact(None).map_err(MemtableError::from_column)?
                };
                self.store(project, day, stored_batch, seq)?;
            }
            return Ok(());
        }

        if matches!(batch.selection(), Selection::All) {
            let mut distinct: Vec<NaiveDate> = active_days.iter().map(|(_, day)| *day).collect();
            distinct.sort_unstable();
            distinct.dedup();
            for day in distinct {
                let mut mask = Bitmap::new_zeroed(batch.raw_len());
                for &(phys, row_day) in &active_days {
                    if row_day == day {
                        mask.set(phys, true).map_err(MemtableError::from_column)?;
                    }
                }
                let filtered = batch
                    .filter_mask(&mask)
                    .and_then(|b| b.compact(None))
                    .map_err(MemtableError::from_column)?;
                self.store(project, day, filtered, seq)?;
            }
        } else {
            for (phys, day) in active_days {
                let idx = u32::try_from(phys).map_err(|_| {
                    MemtableError::from_column(ColumnError::Overflow {
                        reason: "row index exceeds u32::MAX",
                    })
                })?;
                by_day.entry(day).or_default().push(idx);
            }
            for (day, indices) in by_day {
                let filtered = batch
                    .with_selection(Selection::Indices(indices))
                    .and_then(|b| b.compact(None))
                    .map_err(MemtableError::from_column)?;
                self.store(project, day, filtered, seq)?;
            }
        }
        Ok(())
    }

    /// Clones the map of `Arc`s, frozen batches before active ones.
    ///
    /// Cost follows the number of stored batches. A reader cannot tell the two
    /// memtables apart.
    pub(crate) fn snapshot(&self) -> Arc<MemSnapshot> {
        let mut partitions = self
            .frozen
            .as_ref()
            .map(|frozen| frozen.partitions.as_ref().clone())
            .unwrap_or_default();
        for (key, batches) in &self.active.partitions {
            partitions
                .entry(*key)
                .or_default()
                .extend(batches.iter().cloned());
        }
        Arc::new(MemSnapshot { partitions })
    }

    fn store(
        &mut self,
        project: ProjectId,
        day: NaiveDate,
        batch: Batch,
        seq: u64,
    ) -> Result<(), MemtableError> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let stored = MemBatch::from_batch(self.table, batch)?;
        let key = PartitionKey { project, day };
        let partition = self.active.partitions.entry(key).or_default();
        partition.push(stored);
        coalesce(partition)?;
        self.active.note(seq);
        self.active.bytes = self
            .active
            .partitions
            .values()
            .flatten()
            .map(|batch| batch.bytes)
            .sum();
        Ok(())
    }
}

impl MemBatch {
    /// Accounting of `batch`: min/max of the time column, min of `ingest_time`.
    ///
    /// A filtered copy of a batch goes through here too, which recomputes every
    /// field and starts a fresh partials cache.
    pub(crate) fn from_batch(table: Table, batch: Batch) -> Result<Self, MemtableError> {
        let time_name = table.time_column();
        let ingest_name = table.ingest_column();
        let times = ts_column(&batch, time_name)?;
        let ingest = ts_column(&batch, ingest_name)?;
        let mut min_time = i64::MAX;
        let mut max_time = i64::MIN;
        let mut min_ingest_time = i64::MAX;
        for_each_active_row(&batch, |row| {
            let Some(t) = read_ts(times, row) else {
                return Err(MemtableError::BadColumn { column: time_name });
            };
            let Some(ing) = read_ts(ingest, row) else {
                return Err(MemtableError::BadColumn {
                    column: ingest_name,
                });
            };
            min_time = min_time.min(t);
            max_time = max_time.max(t);
            min_ingest_time = min_ingest_time.min(ing);
            Ok(())
        })?;
        let bytes = batch.memory_bytes();
        Ok(Self {
            batch: Arc::new(batch),
            min_time,
            max_time,
            min_ingest_time,
            bytes,
            partials: Arc::new(OnceLock::new()),
        })
    }

    /// Trace partials of this spans batch, computed at most once and cached.
    ///
    /// The `Arc<OnceLock<...>>` is shared by every clone of this `MemBatch`, so
    /// every read state that holds it reuses the same partials. Errors are not
    /// cached (a valid spans batch always builds its partials).
    pub(crate) fn trace_partials(&self) -> Result<Arc<Batch>, crate::error::PartialsError> {
        if let Some(cached) = self.partials.get() {
            return Ok(Arc::clone(cached));
        }
        let built = Arc::new(crate::partials::partials_from_spans_batch(
            self.batch.as_ref(),
        )?);
        // A concurrent reader may have won the race; both values are built from
        // the same immutable batch, so returning either is equivalent.
        let _ = self.partials.set(Arc::clone(&built));
        Ok(Arc::clone(self.partials.get().unwrap_or(&built)))
    }
}

fn ts_column<'a>(batch: &'a Batch, column: &'static str) -> Result<&'a Column, MemtableError> {
    let col = batch
        .column_by_name(column)
        .ok_or(MemtableError::MissingColumn { column })?;
    if !matches!(col.data_type(), DataType::TimestampNs { .. }) {
        return Err(MemtableError::BadColumn { column });
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

fn for_each_active_row(
    batch: &Batch,
    mut f: impl FnMut(usize) -> Result<(), MemtableError>,
) -> Result<(), MemtableError> {
    match batch.selection() {
        Selection::All => {
            for row in 0..batch.raw_len() {
                f(row)?;
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                f(row)?;
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                f(row as usize)?;
            }
        }
    }
    Ok(())
}

fn utc_day(nanos: i64) -> Result<NaiveDate, MemtableError> {
    let days = nanos.div_euclid(DAY_NS);
    let from_ce = days
        .checked_add(UNIX_EPOCH_DAYS_FROM_CE)
        .ok_or(MemtableError::TimeOutOfRange)?;
    let from_ce = i32::try_from(from_ce).map_err(|_| MemtableError::TimeOutOfRange)?;
    NaiveDate::from_num_days_from_ce_opt(from_ce).ok_or(MemtableError::TimeOutOfRange)
}

/// Concatenates every small batch, in the order they appear, into one.
///
/// The coalesced batch takes the place of the first small batch. Large batches
/// stay where they are. A run of only small batches therefore keeps row order.
fn coalesce(batches: &mut Vec<MemBatch>) -> Result<(), MemtableError> {
    let small = batches
        .iter()
        .filter(|batch| batch.bytes < SMALL_BATCH)
        .count();
    if small <= MAX_SMALL_BATCHES {
        return Ok(());
    }
    let mut smalls = Vec::with_capacity(small);
    enum Slot {
        Large(MemBatch),
        Coalesced,
    }
    let mut slots = Vec::with_capacity(batches.len() - small + 1);
    for batch in batches.drain(..) {
        if batch.bytes < SMALL_BATCH {
            if smalls.is_empty() {
                slots.push(Slot::Coalesced);
            }
            smalls.push(batch);
        } else {
            slots.push(Slot::Large(batch));
        }
    }
    let coalesced = concat_small(&smalls)?;
    let mut pending = Some(coalesced);
    *batches = slots
        .into_iter()
        .filter_map(|slot| match slot {
            Slot::Large(batch) => Some(batch),
            Slot::Coalesced => pending.take(),
        })
        .collect();
    Ok(())
}

fn concat_small(smalls: &[MemBatch]) -> Result<MemBatch, MemtableError> {
    let Some(first) = smalls.first() else {
        return Err(MemtableError::from_column(ColumnError::InvalidBatch {
            reason: "no batches to coalesce",
        }));
    };
    let schema = Arc::clone(first.batch.schema());
    let refs: Vec<&Batch> = smalls.iter().map(|batch| batch.batch.as_ref()).collect();
    let merged = Batch::concat(schema, &refs, None).map_err(MemtableError::from_column)?;
    let bytes = merged.memory_bytes();
    let min_time = smalls
        .iter()
        .map(|batch| batch.min_time)
        .min()
        .unwrap_or(first.min_time);
    let max_time = smalls
        .iter()
        .map(|batch| batch.max_time)
        .max()
        .unwrap_or(first.max_time);
    let min_ingest_time = smalls
        .iter()
        .map(|batch| batch.min_ingest_time)
        .min()
        .unwrap_or(first.min_ingest_time);
    Ok(MemBatch {
        batch: Arc::new(merged),
        min_time,
        max_time,
        min_ingest_time,
        bytes,
        partials: Arc::new(OnceLock::new()),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use cetra_core::batch::scores_to_record_batch;
    use cetra_core::enums::ScoreSource;
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::schema::spans::START_TIME;
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use chrono::NaiveDate;

    use super::*;
    use crate::column_bridge::record_batch_to_column;
    use crate::wal::spans_at;

    const DAY: i64 = DAY_NS;

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn other_project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap()
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn spans_col(starts: &[i64]) -> Batch {
        record_batch_to_column(&spans_at(starts)).unwrap()
    }

    fn start_times(batch: &MemBatch) -> Vec<i64> {
        let column = batch.batch.column_by_name(START_TIME).unwrap();
        let mut out = Vec::with_capacity(batch.batch.num_rows());
        for_each_active_row(&batch.batch, |row| {
            out.push(read_ts(column, row).unwrap());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn a_batch_spanning_three_days_lands_in_three_partitions() {
        let mut table = Memtable::new(Table::Spans);
        let batch = spans_col(&[0, 50, DAY, DAY + 10, 2 * DAY]);
        table.insert(project(), batch, 1).unwrap();
        let parts = table.partitions();
        assert_eq!(parts.len(), 3);
        let day0 = &parts[&PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        }];
        let day1 = &parts[&PartitionKey {
            project: project(),
            day: date(1970, 1, 2),
        }];
        let day2 = &parts[&PartitionKey {
            project: project(),
            day: date(1970, 1, 3),
        }];
        assert_eq!(day0[0].batch.num_rows(), 2);
        assert_eq!(day0[0].min_time, 0);
        assert_eq!(day0[0].max_time, 50);
        assert_eq!(day1[0].batch.num_rows(), 2);
        assert_eq!(day1[0].min_time, DAY);
        assert_eq!(day1[0].max_time, DAY + 10);
        assert_eq!(day2[0].batch.num_rows(), 1);
        assert_eq!(day2[0].min_time, 2 * DAY);
        assert_eq!(day2[0].max_time, 2 * DAY);
    }

    #[test]
    fn times_before_1970_land_on_the_previous_day() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[-1]), 1).unwrap();
        table.insert(project(), spans_col(&[-DAY]), 2).unwrap();
        table.insert(project(), spans_col(&[-DAY - 1]), 3).unwrap();
        let parts = table.partitions();
        assert!(parts.contains_key(&PartitionKey {
            project: project(),
            day: date(1969, 12, 31),
        }));
        assert!(parts.contains_key(&PartitionKey {
            project: project(),
            day: date(1969, 12, 30),
        }));
        let dec_31 = &parts[&PartitionKey {
            project: project(),
            day: date(1969, 12, 31),
        }];
        assert_eq!(dec_31.len(), 2);
        assert_eq!(dec_31[0].min_time, -1);
        assert_eq!(dec_31[1].min_time, -DAY);
    }

    #[test]
    fn scores_use_created_at() {
        let mut table = Memtable::new(Table::Scores);
        let score = Score {
            project_id: project(),
            score_id: "018f6b4e-7c1a-7c1a-8c1a-0000000000ab".parse().unwrap(),
            target: ScoreTarget::Trace {
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736"
                    .parse::<TraceId>()
                    .unwrap(),
            },
            session_id: None,
            user_id: None,
            name: "accuracy".to_owned(),
            value: ScoreValue::Numeric(0.5),
            reason: None,
            source: ScoreSource::Api,
            evaluator_id: None,
            rule_id: None,
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: BTreeMap::new(),
            created_at: DAY + 5,
            updated_at: DAY + 6,
            deleted: false,
            ingest_time: DAY + 7,
        };
        let batch = record_batch_to_column(&scores_to_record_batch(&[score]).unwrap()).unwrap();
        table.insert(project(), batch, 1).unwrap();
        assert!(table.partitions().contains_key(&PartitionKey {
            project: project(),
            day: date(1970, 1, 2),
        }));
    }

    #[test]
    fn coalescing_keeps_row_order_and_limits_small_batches() {
        let mut table = Memtable::new(Table::Spans);
        for offset in 0..100 {
            table
                .insert(
                    project(),
                    spans_col(&[offset]),
                    u64::try_from(offset).unwrap(),
                )
                .unwrap();
        }
        let batches = &table.partitions()[&PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        }];
        let small = batches
            .iter()
            .filter(|batch| batch.bytes < SMALL_BATCH)
            .count();
        assert!(small <= MAX_SMALL_BATCHES);
        assert!(batches.len() < 100);
        let times: Vec<i64> = batches.iter().flat_map(start_times).collect();
        assert_eq!(times, (0..100).collect::<Vec<_>>());
        let recomputed: usize = batches.iter().map(|batch| batch.batch.memory_bytes()).sum();
        assert_eq!(table.bytes(), recomputed);
        assert_eq!(
            batches.iter().map(|batch| batch.bytes).sum::<usize>(),
            recomputed
        );
    }

    #[test]
    fn a_snapshot_keeps_the_batches_from_before_coalescing() {
        let mut table = Memtable::new(Table::Spans);
        for _ in 0..10 {
            table.insert(project(), spans_col(&[0]), 1).unwrap();
        }
        let key = PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        };
        let snapshot = table.snapshot();
        let original = Arc::clone(&snapshot.partitions[&key][0].batch);
        for _ in 0..60 {
            table.insert(project(), spans_col(&[0]), 1).unwrap();
        }
        assert_eq!(snapshot.partitions[&key].len(), 10);
        assert!(Arc::ptr_eq(&snapshot.partitions[&key][0].batch, &original));
        assert!(table.partitions()[&key].len() < 70);
        assert!(
            table.partitions()[&key]
                .iter()
                .all(|batch| !Arc::ptr_eq(&batch.batch, &original))
        );
    }

    #[test]
    fn ten_thousand_inserts_do_not_keep_every_batch() {
        let mut table = Memtable::new(Table::Spans);
        for _ in 0..10_000 {
            table.insert(project(), spans_col(&[0]), 1).unwrap();
        }
        let batches = &table.partitions()[&PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        }];
        let small = batches
            .iter()
            .filter(|batch| batch.bytes < SMALL_BATCH)
            .count();
        assert!(small <= MAX_SMALL_BATCHES);
        let rows: usize = batches.iter().map(|batch| batch.batch.num_rows()).sum();
        assert_eq!(rows, 10_000);
        assert!(
            batches.len() <= MAX_SMALL_BATCHES + 8,
            "stored {} batches",
            batches.len()
        );
    }

    #[test]
    fn projects_do_not_share_a_partition() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[0]), 1).unwrap();
        table.insert(other_project(), spans_col(&[0]), 1).unwrap();
        assert_eq!(table.partitions().len(), 2);
    }

    #[test]
    fn extreme_timestamps_do_not_panic() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[i64::MIN]), 1).unwrap();
        table
            .insert(other_project(), spans_col(&[i64::MAX]), 2)
            .unwrap();
        assert_eq!(table.partitions().len(), 2);
    }

    fn key() -> PartitionKey {
        PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        }
    }

    fn snapshot_times(snapshot: &MemSnapshot) -> Vec<i64> {
        snapshot
            .partitions
            .values()
            .flatten()
            .flat_map(start_times)
            .collect()
    }

    fn batch_ptrs(snapshot: &MemSnapshot) -> Vec<*const ()> {
        snapshot
            .partitions
            .values()
            .flatten()
            .map(|batch| Arc::as_ptr(&batch.batch) as *const ())
            .collect()
    }

    #[test]
    fn freeze_moves_every_row_and_snapshots_keep_them() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[1, 2]), 4).unwrap();
        table.insert(project(), spans_col(&[3]), 7).unwrap();
        let before = table.snapshot();
        let bytes = table.bytes();
        let view = table.freeze().expect("active rows");
        let after = table.snapshot();
        assert_eq!(snapshot_times(&before), vec![1, 2, 3]);
        assert_eq!(snapshot_times(&after), snapshot_times(&before));
        assert_eq!(batch_ptrs(&after), batch_ptrs(&before));
        assert!(table.partitions().is_empty());
        assert_eq!(table.bytes(), bytes);
        assert_eq!(table.active_bytes(), 0);
        assert_eq!(view.max_seq, 7);
        assert_eq!(table.active_max_seq(), 0);
        assert!(table.active_oldest().is_none());
    }

    #[test]
    fn inserts_after_a_freeze_stay_active_and_a_retry_returns_the_same_view() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[1]), 4).unwrap();
        let view = table.freeze().expect("active rows");
        table.insert(project(), spans_col(&[2]), 9).unwrap();
        assert_eq!(start_times(&table.partitions()[&key()][0]), vec![2]);
        assert_eq!(snapshot_times(&table.snapshot()), vec![1, 2]);
        let again = table.freeze().expect("frozen rows");
        assert!(Arc::ptr_eq(&view.partitions, &again.partitions));
        assert_eq!(again.max_seq, 4);
        assert_eq!(snapshot_times(&table.snapshot()), vec![1, 2]);
        assert_eq!(table.active_max_seq(), 9);
    }

    #[test]
    fn drop_frozen_removes_only_the_frozen_rows() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[1]), 4).unwrap();
        table.freeze().expect("active rows");
        table.insert(project(), spans_col(&[2]), 9).unwrap();
        let active = table.active_bytes();
        table.drop_frozen();
        assert_eq!(snapshot_times(&table.snapshot()), vec![2]);
        assert_eq!(table.bytes(), active);
        assert_eq!(table.active_max_seq(), 9);
        table.drop_frozen();
        assert_eq!(snapshot_times(&table.snapshot()), vec![2]);
    }

    #[test]
    fn bytes_count_the_active_and_frozen_memtables() {
        let mut table = Memtable::new(Table::Spans);
        table.insert(project(), spans_col(&[1]), 1).unwrap();
        let frozen_bytes = table.bytes();
        table.freeze().expect("active rows");
        table.insert(project(), spans_col(&[2]), 2).unwrap();
        assert_eq!(table.bytes(), frozen_bytes + table.active_bytes());
        assert!(table.active_bytes() > 0);
        assert!(table.bytes() > table.active_bytes());
    }

    #[test]
    fn max_seq_and_oldest_follow_the_active_memtable() {
        let mut table = Memtable::new(Table::Spans);
        assert!(table.freeze().is_none());
        assert!(table.active_oldest().is_none());
        table.insert(project(), spans_col(&[]), 99).unwrap();
        assert!(table.active_oldest().is_none());
        assert_eq!(table.active_max_seq(), 0);
        let started = Instant::now();
        table.insert(project(), spans_col(&[1]), 3).unwrap();
        let oldest = table.active_oldest().expect("first insert");
        assert!(oldest >= started);
        table.insert(project(), spans_col(&[2]), 8).unwrap();
        assert_eq!(table.active_oldest(), Some(oldest));
        table.insert(project(), spans_col(&[3]), 5).unwrap();
        assert_eq!(table.active_max_seq(), 8);
        let view = table.freeze().expect("active rows");
        assert_eq!(view.max_seq, 8);
        assert!(table.active_oldest().is_none());
        assert_eq!(table.active_max_seq(), 0);
        table.insert(project(), spans_col(&[4]), 11).unwrap();
        let restarted = table.active_oldest().expect("new active memtable");
        assert!(restarted >= oldest);
        assert_eq!(table.active_max_seq(), 11);
    }

    #[test]
    fn handles_non_flat_encodings_and_non_all_selection() {
        let mut table = Memtable::new(Table::Spans);
        let base = spans_col(&[10, DAY + 20, 30]);
        let mut cols = base.columns().to_vec();
        let ingest_idx = base
            .schema()
            .index_of(Table::Spans.ingest_column())
            .unwrap();
        cols[ingest_idx] = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: 555,
                utc: true,
            },
            3,
        )
        .unwrap();
        let filtered = Batch::try_new(
            Arc::clone(base.schema()),
            cols,
            3,
            Selection::Indices(vec![2, 0]),
        )
        .unwrap();
        table.insert(project(), filtered, 7).unwrap();
        let parts = table.partitions();
        assert_eq!(parts.len(), 1);
        let stored = &parts[&PartitionKey {
            project: project(),
            day: date(1970, 1, 1),
        }][0];
        assert_eq!(start_times(stored), vec![30, 10]);
        assert_eq!(stored.min_time, 10);
        assert_eq!(stored.max_time, 30);
        assert_eq!(stored.min_ingest_time, 555);
    }
}
