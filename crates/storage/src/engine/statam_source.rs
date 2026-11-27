//! Native Statam `ScanSource` with metadata caching, SBBF/zone-map/trigram/shredded-map
//! pushdown, mini-block streaming, inline tombstones, cooperative cancellation,
//! exact memory reservation accounting, and corrupt-file `Suspects` reporting.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use cetra_column::{
    Batch, CancelToken, Column, ColumnError, DataType, Reservation, ScalarValue, Schema, Selection,
};
use cetra_core::schema::traces;
use cetra_query::exec::source::collect_expr_map_projections;
use cetra_query::exec::{
    ExecContext, IdLookupHint, MapProjection, PartitionStream, ScanSource, ScanSpec,
};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, UnaryOp, covered_predicate};
use cetra_statam::{
    ChunkPackReader, CoalesceMode, IdLookup, LocalityGroup, PackResolver, PackToc, Predicate,
    ScanRequest, StatamError, StatamFileMeta, StatamReader, StatamScanIter, SubstringQuery,
    TimeRange, ZonePred, ZoneValue,
};
use chrono::NaiveDate;

use crate::catalog::{FileId, FileMeta, FileTable, PackMeta};
use crate::deletion::{PendingDeletion, applicable};
use crate::objects::StorageIo;
use crate::statam_io::io::open_range_reader;
use crate::statam_io::time_column;
use crate::unwind::guard_sync;
use crate::verify::Suspects;

use super::metadata_cache::MetadataCache;
use super::{
    DAY_NS, apply_tombstones_and_filters, day_of_ns, reconcile_res, rollback_res,
    to_tombstone_specs, tombstone_table,
};

/// Native Statam [`ScanSource`].
#[derive(Clone)]
pub(crate) struct StatamSource {
    io: StorageIo,
    table: FileTable,
    schema: Arc<Schema>,
    files: Vec<Arc<FileMeta>>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    /// When `true`, files whose mini-blocks exceed `exec_ctx.batch_size` are read in
    /// `exec_ctx.batch_size` row windows via `take_rows` (used by k-way compaction merge so
    /// many concurrent input streams stay within `compaction.max_memory`).
    chunk_to_batch_size: bool,
}

impl fmt::Debug for StatamSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamSource")
            .field("table", &self.table.as_str())
            .field("files", &self.files.len())
            .field("deletions", &self.deletions.len())
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

impl StatamSource {
    /// Creates a [`StatamSource`] over `files` backed by `io`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        io: StorageIo,
        table: FileTable,
        schema: Arc<Schema>,
        files: Vec<Arc<FileMeta>>,
        deletions: Arc<[Arc<PendingDeletion>]>,
        metadata_cache: Arc<MetadataCache>,
        suspects: Suspects,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            io,
            table,
            schema,
            files,
            deletions,
            metadata_cache,
            suspects,
            runtime,
            chunk_to_batch_size: false,
        }
    }

    /// Bounds per-batch decoded rows to `exec_ctx.batch_size` when a file's mini-blocks
    /// are larger than `exec_ctx.batch_size` (used by compaction's k-way merge).
    pub(crate) fn with_chunk_to_batch_size(mut self) -> Self {
        self.chunk_to_batch_size = true;
        self
    }

    fn prune_files(&self, spec: &ScanSpec) -> Vec<Arc<FileMeta>> {
        let min_ns = spec.time_range.min_ns;
        let max_ns = spec.time_range.max_ns;
        if let (Some(lo), Some(hi)) = (min_ns, max_ns)
            && lo > hi
        {
            return Vec::new();
        }
        if spec.id_lookups.iter().any(id_lookup_is_empty) {
            return Vec::new();
        }

        let (day_start, day_end) = file_day_bounds(self.table, min_ns, max_ns);

        // `around_ns` applies ONLY to `Spans`/`Traces` when `spec.id_lookups` contains a
        // `trace_id` `Fixed16` lookup and `time_range` is open (D154).
        let has_trace_lookup = spec.id_lookups.iter().any(|h| {
            matches!(
                h,
                IdLookupHint::Fixed16 { column, .. } if column == traces::TRACE_ID
            )
        });
        let around_window = if self.table != FileTable::Scores
            && has_trace_lookup
            && min_ns.is_none()
            && max_ns.is_none()
        {
            spec.time_range
                .around_ns
                .map(|t| (t.saturating_sub(DAY_NS), t.saturating_add(DAY_NS)))
        } else {
            None
        };

        self.files
            .iter()
            .filter(|file| {
                if file.rows == 0 {
                    return false;
                }
                if file.day < day_start || file.day > day_end {
                    return false;
                }
                if !file_overlaps(file, min_ns, max_ns) {
                    return false;
                }
                if let Some((lo, hi)) = around_window
                    && !file_overlaps(file, Some(lo), Some(hi))
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }
}

impl ScanSource for StatamSource {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
        match &spec.projection {
            Some(indices) => Ok(Arc::new(self.schema.project(indices)?)),
            None => Ok(Arc::clone(&self.schema)),
        }
    }

    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let projected_schema = self.projected_schema(spec)?;
        if spec.limit == Some(0) {
            return Ok(Vec::new());
        }
        let kept_files = self.prune_files(spec);
        if kept_files.is_empty() {
            return Ok(Vec::new());
        }
        self.suspects.record(&kept_files);

        let plan = Arc::new(CompiledScanSpec::compile(
            self.table,
            &self.schema,
            projected_schema,
            spec,
        )?);
        let groups = distribute_files(kept_files, ctx.target_partitions.max(1));
        let streams = groups
            .into_iter()
            .map(|files| {
                Box::new(StatamPartitionStream {
                    io: self.io.clone(),
                    table: self.table,
                    schema: Arc::clone(&self.schema),
                    files: files.into(),
                    deletions: Arc::clone(&self.deletions),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    suspects: self.suspects.clone(),
                    runtime: self.runtime.clone(),
                    plan: Arc::clone(&plan),
                    remaining: plan.stream_limit,
                    eval: ctx.eval.clone(),
                    batch_size: ctx.batch_size.max(1),
                    chunk_to_batch_size: self.chunk_to_batch_size,
                    active_file: None,
                }) as Box<dyn PartitionStream>
            })
            .collect();
        Ok(streams)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        format!(
            "StatamSource(table={}, files={}, filters={}, deletions={})",
            self.table.as_str(),
            self.files.len(),
            spec.filters.len(),
            self.deletions.len()
        )
    }
}

fn id_lookup_is_empty(hint: &IdLookupHint) -> bool {
    match hint {
        IdLookupHint::Fixed16 { values, .. } => values.is_empty(),
        IdLookupHint::Fixed8 { values, .. } => values.is_empty(),
        IdLookupHint::Utf8 { values, .. } => values.is_empty(),
    }
}

fn file_day_bounds(
    table: FileTable,
    min_ns: Option<i64>,
    max_ns: Option<i64>,
) -> (NaiveDate, NaiveDate) {
    let start = min_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MIN);
    let end = max_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MAX);
    if start > end {
        return (NaiveDate::MIN, NaiveDate::MAX);
    }
    match table {
        FileTable::Spans | FileTable::Traces => (
            start.pred_opt().unwrap_or(NaiveDate::MIN),
            end.succ_opt().unwrap_or(NaiveDate::MAX),
        ),
        FileTable::Scores => (start, end),
    }
}

fn file_overlaps(file: &FileMeta, min_ns: Option<i64>, max_ns: Option<i64>) -> bool {
    if let Some(min) = min_ns
        && file.max_time < min
    {
        return false;
    }
    if let Some(max) = max_ns
        && file.min_time > max
    {
        return false;
    }
    true
}

fn distribute_files(mut files: Vec<Arc<FileMeta>>, target: usize) -> Vec<Vec<Arc<FileMeta>>> {
    let slots = target.max(1).min(files.len().max(1));
    files.sort_by(|left, right| {
        right
            .total_bytes()
            .cmp(&left.total_bytes())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut groups: Vec<(u64, Vec<Arc<FileMeta>>)> = (0..slots).map(|_| (0, Vec::new())).collect();
    for file in files {
        let mut best = 0;
        for (idx, (sz, _)) in groups.iter().enumerate().skip(1) {
            if *sz < groups[best].0 {
                best = idx;
            }
        }
        groups[best].0 = groups[best].0.saturating_add(file.total_bytes());
        groups[best].1.push(file);
    }
    groups
        .into_iter()
        .filter(|(_, g)| !g.is_empty())
        .map(|(_, g)| g)
        .collect()
}

/// Precompiled table-level scan plan shared by all partitions of a scan.
struct CompiledScanSpec {
    projection: Option<Vec<usize>>,
    projected_schema: Arc<Schema>,
    output_cols: BTreeSet<usize>,
    filters: Vec<Expr>,
    filter_cols: BTreeSet<usize>,
    base_map_projections: BTreeMap<String, MapProjection>,
    time_range: TimeRange,
    id_lookups: Vec<IdLookup>,
    predicates: Vec<Predicate>,
    substrings: Vec<SubstringQuery>,
    /// True only when the query's sort hint matches the single-key descending
    /// time-column fast path on `Traces` / `Scores` (Plan B4).
    eligible_sort_desc: bool,
    has_sort_hints: bool,
    limit: Option<usize>,
    /// Stream-level row cap enforced only when `sort_hints` is empty (unordered limit).
    stream_limit: Option<usize>,
}

impl CompiledScanSpec {
    fn compile(
        table: FileTable,
        schema: &Arc<Schema>,
        projected_schema: Arc<Schema>,
        spec: &ScanSpec,
    ) -> Result<Self, QueryError> {
        let mut output_cols = BTreeSet::new();
        match &spec.projection {
            Some(indices) => {
                for &idx in indices {
                    if idx >= schema.len() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "projection index out of bounds",
                        }
                        .into());
                    }
                    output_cols.insert(idx);
                }
            }
            None => {
                for idx in 0..schema.len() {
                    output_cols.insert(idx);
                }
            }
        }

        let mut filter_cols = BTreeSet::new();
        for filter in &spec.filters {
            filter.referenced_columns(&mut filter_cols);
        }

        // B3: Compute merged `MapProjection` lattice over projected `MapUtf8` columns
        // (treating any projected `MapUtf8` column absent from `spec.map_projections` as `Full`)
        // plus all filter references. Per-file tombstones are merged later in `build_file_scan`.
        let mut base_map_projections: BTreeMap<String, MapProjection> = BTreeMap::new();
        for &cidx in &output_cols {
            let field = &schema.fields()[cidx];
            if field.data_type == DataType::MapUtf8 {
                let proj = spec
                    .map_projections
                    .get(&field.name)
                    .cloned()
                    .unwrap_or(MapProjection::Full);
                base_map_projections.insert(field.name.clone(), proj);
            }
        }
        for filter in &spec.filters {
            collect_expr_map_projections(filter, schema, &mut base_map_projections);
        }

        // Time range hint.
        let time_range = TimeRange {
            min_ns: spec.time_range.min_ns.unwrap_or(i64::MIN),
            max_ns: spec.time_range.max_ns.unwrap_or(i64::MAX),
        };

        // ID lookups (schema-validated; ExternalPayload columns filtered per-file).
        let mut id_lookups = Vec::with_capacity(spec.id_lookups.len());
        for hint in &spec.id_lookups {
            let (column, values): (&str, Vec<Vec<u8>>) = match hint {
                IdLookupHint::Fixed16 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Fixed8 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Utf8 { column, values } => (
                    column,
                    values.iter().map(|s| s.as_bytes().to_vec()).collect(),
                ),
            };
            if let Some(cidx) = schema.index_of(column)
                && is_supported_id_lookup_type(&schema.fields()[cidx].data_type)
            {
                id_lookups.push(IdLookup::in_list(column.to_owned(), values));
            }
        }

        // Pushdown-safe predicates extracted from `spec.filters` (residual filters remain exact).
        let mut predicates = Vec::new();
        for filter in &spec.filters {
            extract_predicates(filter, schema, &mut predicates);
        }

        // B2: Push ONLY single-disjunct CNF clauses targeting `DataType::Utf8`.
        // Multi-disjunct OR clauses (`clause.len() > 1`) stay in `spec.filters`.
        let mut substrings = Vec::new();
        for clause in &spec.substrings {
            if clause.len() != 1 {
                continue;
            }
            let hint = &clause[0];
            if let Some(cidx) = schema.index_of(&hint.column)
                && schema.fields()[cidx].data_type == DataType::Utf8
            {
                substrings.push(SubstringQuery {
                    column: hint.column.clone(),
                    needle: hint.needle.as_bytes().to_vec(),
                    case_insensitive: hint.case_insensitive,
                });
            }
        }

        // B4: `sort_desc_hint` eligibility (exact single-key descending time column on Traces/Scores).
        let eligible_sort_desc = matches!(table, FileTable::Traces | FileTable::Scores)
            && spec.sort_hints.len() == 1
            && spec.sort_hints[0].descending
            && spec.sort_hints[0].column == time_column(table);
        let has_sort_hints = !spec.sort_hints.is_empty();
        // Enforce a stream-level cap only when there are no sort hints (unordered LIMIT)
        // so ordered scans with residual filters/tombstones never drop qualifying top rows.
        let stream_limit = if !has_sort_hints { spec.limit } else { None };

        Ok(Self {
            projection: spec.projection.clone(),
            projected_schema,
            output_cols,
            filters: spec.filters.clone(),
            filter_cols,
            base_map_projections,
            time_range,
            id_lookups,
            predicates,
            substrings,
            eligible_sort_desc,
            has_sort_hints,
            limit: spec.limit,
            stream_limit,
        })
    }
}

fn is_supported_id_lookup_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::FixedBytes16 | DataType::FixedBytes8 | DataType::Utf8 | DataType::Binary
    )
}

fn scalar_to_zone_value(sv: &ScalarValue, col_dt: &DataType) -> Option<ZoneValue> {
    match (sv, col_dt) {
        (ScalarValue::Bool(b), DataType::Bool) => Some(ZoneValue::Bool(*b)),
        (ScalarValue::I64(v), DataType::I64) => Some(ZoneValue::I64(*v)),
        (ScalarValue::F64(f), DataType::F64) if !f.is_nan() => Some(ZoneValue::F64(*f)),
        (
            ScalarValue::Decimal128 {
                value,
                precision,
                scale,
            },
            DataType::Decimal128 {
                precision: cp,
                scale: cs,
            },
        ) if precision == cp && scale == cs => Some(ZoneValue::Decimal128(*value)),
        (ScalarValue::TimestampNs { value, .. }, DataType::TimestampNs { .. }) => {
            Some(ZoneValue::TimestampNs(*value))
        }
        (ScalarValue::Date(d), DataType::Date) => Some(ZoneValue::Date(*d)),
        (ScalarValue::Utf8(s), DataType::Utf8) => Some(ZoneValue::Bytes(s.as_bytes().to_vec())),
        (ScalarValue::Binary(b), DataType::Binary) => Some(ZoneValue::Bytes(b.clone())),
        (ScalarValue::FixedBytes8(b), DataType::FixedBytes8) => Some(ZoneValue::Bytes(b.to_vec())),
        (ScalarValue::FixedBytes16(b), DataType::FixedBytes16) => {
            Some(ZoneValue::Bytes(b.to_vec()))
        }
        _ => None,
    }
}

fn flip_cmp(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Eq => Some(BinaryOp::Eq),
        BinaryOp::NotEq => Some(BinaryOp::NotEq),
        BinaryOp::Lt => Some(BinaryOp::Gt),
        BinaryOp::LtEq => Some(BinaryOp::GtEq),
        BinaryOp::Gt => Some(BinaryOp::Lt),
        BinaryOp::GtEq => Some(BinaryOp::LtEq),
        _ => None,
    }
}

fn cmp_to_zone_pred(op: BinaryOp, val: ZoneValue) -> Option<ZonePred> {
    match op {
        BinaryOp::Eq => Some(ZonePred::Eq(val)),
        BinaryOp::NotEq => Some(ZonePred::Ne(val)),
        BinaryOp::Lt => Some(ZonePred::Lt(val)),
        BinaryOp::LtEq => Some(ZonePred::Le(val)),
        BinaryOp::Gt => Some(ZonePred::Gt(val)),
        BinaryOp::GtEq => Some(ZonePred::Ge(val)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum PredTarget<'a> {
    Col { name: &'a str, dt: &'a DataType },
    MapKey { name: &'a str, key: &'a str },
}

fn extract_target<'a>(expr: &'a Expr, schema: &'a Schema) -> Option<PredTarget<'a>> {
    match expr {
        Expr::Column { index, .. } => {
            let field = schema.fields().get(*index)?;
            Some(PredTarget::Col {
                name: field.name.as_str(),
                dt: &field.data_type,
            })
        }
        Expr::MapIndex { map, key } => {
            let Expr::Column { index, .. } = map.as_ref() else {
                return None;
            };
            let Expr::Literal(ScalarValue::Utf8(k)) = key.as_ref() else {
                return None;
            };
            let field = schema.fields().get(*index)?;
            if field.data_type != DataType::MapUtf8 {
                return None;
            }
            Some(PredTarget::MapKey {
                name: field.name.as_str(),
                key: k.as_str(),
            })
        }
        _ => None,
    }
}

fn push_target_pred(target: PredTarget<'_>, pred: ZonePred, out: &mut Vec<Predicate>) {
    match target {
        PredTarget::Col { name, .. } => out.push(Predicate::new(name.to_owned(), pred)),
        PredTarget::MapKey { name, key } => {
            out.push(Predicate::map_key(name.to_owned(), key.to_owned(), pred));
        }
    }
}

fn target_value(target: &PredTarget<'_>, sv: &ScalarValue) -> Option<ZoneValue> {
    match target {
        PredTarget::Col { dt, .. } => scalar_to_zone_value(sv, dt),
        PredTarget::MapKey { .. } => scalar_to_zone_value(sv, &DataType::Utf8),
    }
}

fn extract_predicates(expr: &Expr, schema: &Schema, out: &mut Vec<Predicate>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            extract_predicates(left, schema, out);
            extract_predicates(right, schema, out);
        }
        Expr::Binary {
            left, op, right, ..
        } => {
            let (target, cmp_op, sv) = if let (Some(t), Expr::Literal(sv)) =
                (extract_target(left, schema), right.as_ref())
            {
                (t, *op, sv)
            } else if let (Expr::Literal(sv), Some(t)) =
                (left.as_ref(), extract_target(right, schema))
            {
                let Some(flipped) = flip_cmp(*op) else {
                    return;
                };
                (t, flipped, sv)
            } else {
                return;
            };
            if let Some(val) = target_value(&target, sv)
                && let Some(zpred) = cmp_to_zone_pred(cmp_op, val)
            {
                push_target_pred(target, zpred, out);
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let Some(target) = extract_target(expr, schema) else {
                return;
            };
            let (Expr::Literal(lo_sv), Expr::Literal(hi_sv)) = (low.as_ref(), high.as_ref()) else {
                return;
            };
            if let (Some(lo), Some(hi)) =
                (target_value(&target, lo_sv), target_value(&target, hi_sv))
            {
                push_target_pred(target, ZonePred::Between(lo, hi), out);
            }
        }
        Expr::InList {
            expr,
            list,
            negated: false,
            ..
        } => {
            let Some(target) = extract_target(expr, schema) else {
                return;
            };
            if list.is_empty() {
                return;
            }
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                let Expr::Literal(sv) = item else {
                    return;
                };
                if sv.is_null() {
                    return;
                }
                let Some(zv) = target_value(&target, sv) else {
                    return;
                };
                vals.push(zv);
            }
            if !vals.is_empty() {
                push_target_pred(target, ZonePred::In(vals), out);
            }
        }
        Expr::Unary {
            op: UnaryOp::IsNull,
            expr,
            ..
        } => {
            if let Some(target) = extract_target(expr, schema) {
                push_target_pred(target, ZonePred::IsNull, out);
            }
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr,
            ..
        } => {
            if let Some(target) = extract_target(expr, schema) {
                push_target_pred(target, ZonePred::IsNotNull, out);
            }
        }
        _ => {}
    }
}

/// Resolves the single sibling `.stpk` pack owned 1:1 by an L1 `spans` file,
/// caching its decoded [`PackToc`] in [`MetadataCache`].
struct SingleFilePackResolver {
    pack: PackMeta,
    io: StorageIo,
    metadata_cache: Arc<MetadataCache>,
    runtime: tokio::runtime::Handle,
    cancel: CancelToken,
}

impl PackResolver for SingleFilePackResolver {
    fn resolve(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError> {
        if pack_id != self.pack.id {
            return Err(StatamError::MissingPack { pack_id });
        }
        let rr = open_range_reader(
            &self.io,
            &self.pack.path,
            self.pack.size,
            self.runtime.clone(),
            self.cancel.clone(),
        )?;
        let toc = if let Some(cached) = self.metadata_cache.get_pack(pack_id) {
            cached
        } else {
            let (toc, _retained) =
                PackToc::read_from_with_hint(rr.as_ref(), Some(self.pack.toc_size))?;
            self.metadata_cache.insert_pack(pack_id, Arc::clone(&toc));
            toc
        };
        let reader = ChunkPackReader::open_with_toc(rr, toc)?;
        Ok(Arc::new(reader))
    }
}

enum FileScanCursor {
    Iter(Box<StatamScanIter>),
    TakeRows {
        reader: Arc<StatamReader>,
        projection: Option<Vec<usize>>,
        ranges: VecDeque<(u64, u64)>,
        buffered: VecDeque<Batch>,
        batch_size: usize,
    },
}

impl FileScanCursor {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, StatamError> {
        match self {
            Self::Iter(iter) => guard_sync(|| iter.next_batch(Some(cancel), res)),
            Self::TakeRows {
                reader,
                projection,
                ranges,
                buffered,
                batch_size,
            } => {
                let res_before = res.as_ref().map(|r| r.size());
                loop {
                    if cancel.is_cancelled() {
                        return Err(StatamError::Cancelled);
                    }
                    if let Some(batch) = buffered.pop_front() {
                        if batch.is_empty() {
                            continue;
                        }
                        let mem = batch.memory_bytes();
                        if let Some(r) = res.as_deref_mut()
                            && r.try_grow(mem).is_err()
                        {
                            if let Some(before) = res_before
                                && r.size() > before
                            {
                                r.shrink(r.size() - before);
                            }
                            return Err(StatamError::MemoryExceeded);
                        }
                        return Ok(Some(batch));
                    }
                    let mut row_ids: Vec<u64> = Vec::with_capacity(*batch_size);
                    while row_ids.len() < *batch_size {
                        let Some((lo, hi)) = ranges.front_mut() else {
                            break;
                        };
                        let need = (*batch_size - row_ids.len()) as u64;
                        let avail = hi.saturating_sub(*lo);
                        let take = need.min(avail);
                        for r in *lo..(*lo + take) {
                            row_ids.push(r);
                        }
                        *lo += take;
                        if *lo >= *hi {
                            ranges.pop_front();
                        }
                    }
                    if row_ids.is_empty() {
                        return Ok(None);
                    }
                    let batches =
                        guard_sync(|| reader.take_rows(&row_ids, projection.as_deref(), 1))?;
                    for b in batches {
                        if !b.is_empty() {
                            buffered.push_back(b);
                        }
                    }
                }
            }
        }
    }
}

enum ScanColSlot {
    FromFile(usize),
    FillNull(DataType),
}

struct ScanBatchFill {
    schema: Arc<Schema>,
    slots: Vec<ScanColSlot>,
}

struct ActiveFileScan {
    file_id: FileId,
    cursor: FileScanCursor,
    fill_layout: Option<ScanBatchFill>,
    tombstone_pred: Option<Expr>,
    remapped_filters: Vec<Expr>,
    /// Maps each output column slot to its position in the scanned batch (`None` if identity).
    scan_to_output: Option<Vec<usize>>,
}

struct StatamPartitionStream {
    io: StorageIo,
    table: FileTable,
    schema: Arc<Schema>,
    files: VecDeque<Arc<FileMeta>>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    plan: Arc<CompiledScanSpec>,
    remaining: Option<usize>,
    eval: EvalContext,
    batch_size: usize,
    chunk_to_batch_size: bool,
    active_file: Option<ActiveFileScan>,
}

impl StatamPartitionStream {
    fn map_statam_err(&self, file_id: FileId, err: StatamError) -> QueryError {
        if err.is_corrupt() {
            self.suspects.report_corrupt(file_id);
            return QueryError::CorruptData("corrupt statam file".to_owned());
        }
        if err.is_store_error() {
            return QueryError::StorageIo("storage read error".to_owned());
        }
        match err {
            StatamError::Cancelled => QueryError::Cancelled,
            StatamError::MemoryExceeded => QueryError::TooLarge,
            StatamError::Column(c) => QueryError::Column(c),
            _ => QueryError::internal("statam read failed"),
        }
    }

    fn open_next_file(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<ActiveFileScan>, QueryError> {
        'next_file: while let Some(file) = self.files.pop_front() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }

            let rr = open_range_reader(
                &self.io,
                &file.path,
                file.size,
                self.runtime.clone(),
                cancel.clone(),
            )
            .map_err(|e| self.map_statam_err(file.id, e))?;

            let meta = if let Some(cached) = self.metadata_cache.get_statam(file.id) {
                cached
            } else {
                let loaded = guard_sync(|| {
                    StatamFileMeta::read_from_with_hint(rr.as_ref(), Some(file.footer_size))
                })
                .map_err(|e| self.map_statam_err(file.id, e))?;
                let meta = Arc::new(loaded);
                self.metadata_cache
                    .insert_statam(file.id, Arc::clone(&meta));
                meta
            };

            // Resolve every target schema column against the file's schema by name
            // (spec 03 §3 schema evolution: missing nullable columns become `Const(Null)`;
            // missing non-nullable columns or type mismatches are corrupt).
            let file_schema = &meta.schema_meta.schema;
            let mut full_to_file: Vec<Option<usize>> = Vec::with_capacity(self.schema.len());
            for field in self.schema.fields() {
                match file_schema.index_of(&field.name) {
                    Some(file_idx) => {
                        if file_schema.fields()[file_idx].data_type != field.data_type {
                            self.suspects.report_corrupt(file.id);
                            return Err(QueryError::CorruptData(
                                "column type mismatch in statam file".to_owned(),
                            ));
                        }
                        full_to_file.push(Some(file_idx));
                    }
                    None => {
                        if !field.nullable {
                            self.suspects.report_corrupt(file.id);
                            return Err(QueryError::CorruptData(
                                "missing non-nullable column in statam file".to_owned(),
                            ));
                        }
                        full_to_file.push(None);
                    }
                }
            }

            // Filter pushdowns against `file_schema` (short-circuiting this file to 0 rows
            // whenever a conjunctive lookup/predicate/substring targets a missing nullable
            // all-NULL column, and dropping `IS NULL` predicates on missing nullable columns).
            let mut id_lookups: Vec<IdLookup> = Vec::with_capacity(self.plan.id_lookups.len());
            for lk in &self.plan.id_lookups {
                let Some(cidx) = file_schema.index_of(&lk.column) else {
                    continue 'next_file;
                };
                if meta
                    .col_dir
                    .get(cidx)
                    .is_some_and(|cd| cd.locality_group != LocalityGroup::ExternalPayload)
                {
                    id_lookups.push(lk.clone());
                }
            }

            let mut predicates: Vec<Predicate> = Vec::with_capacity(self.plan.predicates.len());
            for p in &self.plan.predicates {
                match file_schema.index_of(&p.column) {
                    None => {
                        if !matches!(p.pred, ZonePred::IsNull) {
                            continue 'next_file;
                        }
                    }
                    Some(cidx) => {
                        if meta.col_dir.get(cidx).is_some_and(|cd| {
                            cd.locality_group != LocalityGroup::ExternalPayload
                                || matches!(p.pred, ZonePred::IsNull | ZonePred::IsNotNull)
                        }) {
                            predicates.push(p.clone());
                        }
                    }
                }
            }

            let mut substrings: Vec<SubstringQuery> =
                Vec::with_capacity(self.plan.substrings.len());
            for q in &self.plan.substrings {
                if file_schema.index_of(&q.column).is_none() {
                    continue 'next_file;
                }
                substrings.push(q.clone());
            }

            let packs: Option<Arc<dyn PackResolver>> = file.pack.as_ref().map(|pack| {
                Arc::new(SingleFilePackResolver {
                    pack: pack.clone(),
                    io: self.io.clone(),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    runtime: self.runtime.clone(),
                    cancel: cancel.clone(),
                }) as Arc<dyn PackResolver>
            });

            let reader = Arc::new(
                guard_sync(|| StatamReader::open_with_shared_meta(rr, Arc::clone(&meta), packs))
                    .map_err(|e| self.map_statam_err(file.id, e))?,
            );

            let file_deletions = applicable(
                &self.deletions,
                Some(file.deletions_applied_seq),
                file.min_ingest_time,
            );
            let tombstone_full = if file_deletions.is_empty() {
                None
            } else {
                let specs = to_tombstone_specs(&file_deletions);
                covered_predicate(tombstone_table(self.table), &self.schema, &specs)?
            };

            let mut scan_set = self.plan.output_cols.clone();
            scan_set.extend(self.plan.filter_cols.iter().copied());
            if let Some(ref pred) = tombstone_full {
                pred.referenced_columns(&mut scan_set);
            }

            let scan_indices: Vec<usize> = scan_set.into_iter().collect();
            let mut file_proj = Vec::with_capacity(scan_indices.len());
            let mut slots = Vec::with_capacity(scan_indices.len());
            let mut has_missing_col = false;
            for &full_idx in &scan_indices {
                match full_to_file[full_idx] {
                    Some(file_idx) => {
                        let file_pos = file_proj.len();
                        file_proj.push(file_idx);
                        slots.push(ScanColSlot::FromFile(file_pos));
                    }
                    None => {
                        has_missing_col = true;
                        slots.push(ScanColSlot::FillNull(
                            self.schema.fields()[full_idx].data_type.clone(),
                        ));
                    }
                }
            }

            let is_all_in_order = file_proj.len() == file_schema.len()
                && file_proj.iter().enumerate().all(|(i, &c)| i == c);
            let req_projection = if is_all_in_order {
                None
            } else {
                Some(file_proj)
            };
            let fill_layout = if has_missing_col {
                let scan_schema = Arc::new(self.schema.project(&scan_indices)?);
                Some(ScanBatchFill {
                    schema: scan_schema,
                    slots,
                })
            } else {
                None
            };

            let mut full_to_scan: Vec<Option<usize>> = vec![None; self.schema.len()];
            for (scan_pos, &full_idx) in scan_indices.iter().enumerate() {
                full_to_scan[full_idx] = Some(scan_pos);
            }

            let tombstone_pred = match tombstone_full {
                Some(ref expr) => Some(expr.remap_columns(&full_to_scan)?),
                None => None,
            };
            let remapped_filters: Vec<Expr> = self
                .plan
                .filters
                .iter()
                .map(|f| f.remap_columns(&full_to_scan))
                .collect::<Result<Vec<_>, _>>()?;

            let scan_to_output = match &self.plan.projection {
                Some(out_indices) => {
                    let mut map = Vec::with_capacity(out_indices.len());
                    for &full_idx in out_indices {
                        let pos = full_to_scan[full_idx]
                            .ok_or_else(|| QueryError::internal("projected column missing"))?;
                        map.push(pos);
                    }
                    let identity = map.len() == scan_indices.len()
                        && map.iter().enumerate().all(|(i, &p)| i == p);
                    if identity { None } else { Some(map) }
                }
                None => None,
            };

            // B3: Merge per-file tombstone map projections into the base lattice.
            let mut map_projections = self.plan.base_map_projections.clone();
            if let Some(ref pred) = tombstone_full {
                collect_expr_map_projections(pred, &self.schema, &mut map_projections);
            }
            let mut map_keys = Vec::new();
            for (col_name, proj) in &map_projections {
                if let MapProjection::Keys(keys) = proj
                    && !keys.is_empty()
                    && file_schema.index_of(col_name).is_some_and(|cidx| {
                        meta.col_dir.get(cidx).is_some()
                            && file_schema
                                .fields()
                                .get(cidx)
                                .is_some_and(|f| f.data_type == DataType::MapUtf8)
                    })
                {
                    for key in keys {
                        map_keys.push((col_name.clone(), key.clone()));
                    }
                }
            }

            // B4: Exact `sort_desc_hint` and `req.limit` rules.
            let no_residual_or_tombstones = remapped_filters.is_empty() && tombstone_pred.is_none();
            let sort_desc_hint = self.plan.eligible_sort_desc
                && no_residual_or_tombstones
                && self.plan.limit.is_some();
            let req_limit =
                if no_residual_or_tombstones && (!self.plan.has_sort_hints || sort_desc_hint) {
                    if !self.plan.has_sort_hints {
                        self.remaining
                    } else {
                        self.plan.limit
                    }
                } else {
                    None
                };

            let max_mb_rows = meta
                .block_dir
                .iter()
                .map(|b| usize::from(b.row_count))
                .max()
                .unwrap_or(0);
            let use_take_rows = self.chunk_to_batch_size
                && max_mb_rows > self.batch_size
                && id_lookups.is_empty()
                && substrings.is_empty()
                && !sort_desc_hint
                && map_keys.is_empty();

            let cursor = if use_take_rows {
                let mut ranges = VecDeque::new();
                for rg in &meta.rg_dir {
                    let start = u64::from(rg.first_row);
                    let end = start.saturating_add(u64::from(rg.row_count));
                    if start < end {
                        ranges.push_back((start, end));
                    }
                }
                FileScanCursor::TakeRows {
                    reader,
                    projection: req_projection,
                    ranges,
                    buffered: VecDeque::new(),
                    batch_size: self.batch_size,
                }
            } else {
                let req = ScanRequest {
                    projection: req_projection,
                    map_keys,
                    time_range: self.plan.time_range,
                    id_lookups,
                    predicates,
                    substrings,
                    sort_desc_hint,
                    limit: req_limit,
                    coalesce_mode: CoalesceMode::Auto,
                    // B5: Always 1 thread per Statam scan; parallelism lives in `cetra-query::WorkerPool`.
                    read_threads: 1,
                };
                let iter = guard_sync(|| reader.scan_iter(req))
                    .map_err(|e| self.map_statam_err(file.id, e))?;
                FileScanCursor::Iter(Box::new(iter))
            };

            return Ok(Some(ActiveFileScan {
                file_id: file.id,
                cursor,
                fill_layout,
                tombstone_pred,
                remapped_filters,
                scan_to_output,
            }));
        }
        Ok(None)
    }
}

fn fill_missing_columns(batch: &Batch, layout: &ScanBatchFill) -> Result<Batch, QueryError> {
    let raw_len = batch.raw_len();
    let src_cols = batch.columns();
    let mut cols = Vec::with_capacity(layout.slots.len());
    for slot in &layout.slots {
        match slot {
            ScanColSlot::FromFile(idx) => {
                let col = src_cols
                    .get(*idx)
                    .ok_or_else(|| QueryError::internal("scanned column index out of bounds"))?;
                cols.push(Arc::clone(col));
            }
            ScanColSlot::FillNull(dt) => {
                cols.push(Arc::new(Column::Const {
                    value: ScalarValue::Null(dt.clone()),
                    len: raw_len,
                }));
            }
        }
    }
    Ok(Batch::try_new_shared(
        Arc::clone(&layout.schema),
        cols,
        raw_len,
        batch.selection().clone(),
    )?)
}

fn project_scanned_batch(
    batch: Batch,
    projected_schema: &Arc<Schema>,
    scan_to_output: Option<&[usize]>,
) -> Result<Batch, QueryError> {
    let Some(indices) = scan_to_output else {
        return Ok(batch);
    };
    let cols: Vec<Arc<Column>> = indices
        .iter()
        .map(|&idx| Arc::clone(&batch.columns()[idx]))
        .collect();
    let out = Batch::try_new_shared(
        Arc::clone(projected_schema),
        cols,
        batch.raw_len(),
        batch.selection().clone(),
    )?;
    Ok(out)
}

fn truncate_active_rows(batch: Batch, keep: usize) -> Result<Batch, QueryError> {
    if batch.num_rows() <= keep {
        return Ok(batch);
    }
    match batch.selection() {
        Selection::All => Ok(batch.slice(0, keep, None)?),
        Selection::Indices(ids) => {
            let trimmed = ids[..keep].to_vec();
            Ok(batch.into_with_selection(Selection::Indices(trimmed))?)
        }
        Selection::Mask(bm) => {
            let mut trimmed = Vec::with_capacity(keep);
            for row in bm.iter_ones().take(keep) {
                let idx = u32::try_from(row).map_err(|_| QueryError::TooLarge)?;
                trimmed.push(idx);
            }
            Ok(batch.into_with_selection(Selection::Indices(trimmed))?)
        }
    }
}

impl PartitionStream for StatamPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());

        loop {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if self.remaining == Some(0) {
                rollback_res(res, before);
                return Ok(None);
            }

            if self.active_file.is_none() {
                match self.open_next_file(cancel) {
                    Ok(Some(active)) => self.active_file = Some(active),
                    Ok(None) => {
                        rollback_res(res, before);
                        return Ok(None);
                    }
                    Err(err) => {
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            }

            let Some(active) = self.active_file.as_mut() else {
                rollback_res(res, before);
                return Ok(None);
            };
            let file_id = active.file_id;
            let pulled = active.cursor.next_batch(cancel, res.as_deref_mut());
            let raw_batch = match pulled {
                Ok(Some(batch)) => batch,
                Ok(None) => {
                    self.active_file = None;
                    rollback_res(res.as_deref_mut(), before);
                    continue;
                }
                Err(err) => {
                    self.active_file = None;
                    rollback_res(res, before);
                    return Err(self.map_statam_err(file_id, err));
                }
            };

            let raw_batch = match &active.fill_layout {
                Some(layout) => fill_missing_columns(&raw_batch, layout)
                    .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?,
                None => raw_batch,
            };

            let filtered = apply_tombstones_and_filters(
                raw_batch,
                active.tombstone_pred.as_ref(),
                &active.remapped_filters,
                &self.eval,
                res.as_deref_mut(),
                before,
            )?;
            if filtered.is_empty() {
                rollback_res(res.as_deref_mut(), before);
                continue;
            }

            let mut out = project_scanned_batch(
                filtered,
                &self.plan.projected_schema,
                active.scan_to_output.as_deref(),
            )
            .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?;

            if let Some(rem) = self.remaining.as_mut() {
                if out.num_rows() > *rem {
                    out = truncate_active_rows(out, *rem)
                        .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?;
                }
                *rem = rem.saturating_sub(out.num_rows());
            }

            reconcile_res(res, before, out.memory_bytes())?;
            return Ok(Some(out));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use cetra_column::{Batch, CancelToken, DataType, GlobalPool, ScalarValue, Schema};
    use cetra_core::ids::ProjectId;
    use cetra_core::schema::{scores, spans, traces};
    use cetra_query::exec::{
        ExecContext, ExecMetrics, IdLookupHint, MapProjection, ScanSource, ScanSpec, SortHint,
        SpillManager, SubstringHint, TimeRangeHint, WorkerPool,
    };
    use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, ScalarFn};
    use cetra_statam::{
        ChunkPackBuilder, InMemoryPackResolver, PackWriteOptions, SliceReader, StatamReader,
    };
    use chrono::NaiveDate;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use uuid::Uuid;

    use super::*;
    use crate::catalog::{FileId, FileMeta, FileTable, PackMeta};
    use crate::statam_io::{BatchFileWriter, write_file_batch};
    use crate::test_support::{
        Call, ChaosStore, Fault, score_row, score_rows, span_row, span_rows, trace_batch,
    };
    use crate::verify::{Suspects, VerifyQueue};

    /// Decodes a stored `.statam` file (and optional sibling `.stpk` pack bytes) into one `Batch`.
    pub(crate) fn read_stored_statam(
        bytes: Bytes,
        pack: Option<([u8; 16], Bytes)>,
        schema: Arc<Schema>,
    ) -> Batch {
        let rr = Arc::new(SliceReader::new(bytes));
        let packs: Option<Arc<dyn PackResolver>> = pack.map(|(pack_id, pack_bytes)| {
            let mut resolver = InMemoryPackResolver::new();
            resolver
                .insert_bytes(pack_id, pack_bytes)
                .expect("open test pack");
            Arc::new(resolver) as Arc<dyn PackResolver>
        });
        let reader = Arc::new(StatamReader::open(rr, packs).expect("open test statam"));
        let batches = reader
            .scan(&ScanRequest {
                read_threads: 1,
                ..ScanRequest::default()
            })
            .expect("scan test statam");
        if batches.is_empty() {
            return Batch::empty(schema);
        }
        let refs: Vec<&Batch> = batches.iter().collect();
        Batch::concat(schema, &refs, None).expect("concat statam batches")
    }

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, d).unwrap()
    }

    fn ns(day_num: u32, secs: i64) -> i64 {
        let date = day(day_num);
        date.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
            + secs * 1_000_000_000
    }

    fn make_exec_ctx(pool_cap: usize, cancel: CancelToken) -> ExecContext {
        let pool = GlobalPool::standalone(pool_cap);
        let workers = Arc::new(WorkerPool::new(1).unwrap());
        ExecContext {
            pool,
            eval: EvalContext::new(0, cancel),
            spill: Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0)),
            workers,
            batch_size: 8192,
            target_partitions: 1,
            metrics: ExecMetrics::new(),
        }
    }

    async fn put_statam_file(
        store: &Arc<dyn ObjectStore>,
        table: FileTable,
        level: u8,
        d: NaiveDate,
        batch: &Batch,
    ) -> Arc<FileMeta> {
        let written = write_file_batch(table, level, &[Arc::new(batch.clone())]).unwrap();
        let (path, id) = crate::objects::Objects::table_file_path(table, project(), d, level);
        store
            .put(
                &ObjectPath::from(path.as_str()),
                PutPayload::from(written.bytes.clone()),
            )
            .await
            .unwrap();
        Arc::new(FileMeta {
            id,
            table,
            project: project(),
            day: d,
            level,
            path,
            size: written.bytes.len() as u64,
            footer_size: written.footer_size,
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: level == 1,
            deletions_applied_seq: 0,
            pack: None,
        })
    }

    async fn put_l1_spans_with_pack(
        store: &Arc<dyn ObjectStore>,
        d: NaiveDate,
        batch: &Batch,
    ) -> Arc<FileMeta> {
        let (path, id) =
            crate::objects::Objects::table_file_path(FileTable::Spans, project(), d, 1);
        let pack_uuid = Uuid::now_v7();
        let pack_id = *pack_uuid.as_bytes();
        let pack_path = crate::objects::Objects::pack_file_path(project(), d, 1, pack_uuid);
        let pb = ChunkPackBuilder::new_streaming(
            pack_id,
            PackWriteOptions::default(),
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer =
            BatchFileWriter::new_with_pack(FileTable::Spans, 1, Cursor::new(Vec::new()), Some(pb))
                .unwrap();
        writer.write(batch).unwrap();
        let (statam_cursor, summary, pb_opt) = writer.finish().unwrap();
        let statam_bytes = Bytes::from(statam_cursor.into_inner());
        let pb = pb_opt.unwrap();
        let pack_meta = if pb.chunk_count() > 0 {
            let (pack_cursor, pstats) = pb.finish_into_inner().unwrap();
            let pack_bytes = Bytes::from(pack_cursor.into_inner());
            store
                .put(
                    &ObjectPath::from(pack_path.as_str()),
                    PutPayload::from(pack_bytes.clone()),
                )
                .await
                .unwrap();
            Some(PackMeta {
                id: pack_id,
                path: pack_path,
                size: pack_bytes.len() as u64,
                toc_size: u64::from(pstats.toc_stored_bytes),
            })
        } else {
            None
        };
        store
            .put(
                &ObjectPath::from(path.as_str()),
                PutPayload::from(statam_bytes.clone()),
            )
            .await
            .unwrap();
        Arc::new(FileMeta {
            id,
            table: FileTable::Spans,
            project: project(),
            day: d,
            level: 1,
            path,
            size: statam_bytes.len() as u64,
            footer_size: summary.footer_size,
            rows: summary.rows,
            min_time: summary.min_time,
            max_time: summary.max_time,
            min_ingest_time: summary.min_ingest_time,
            max_ingest_time: summary.max_ingest_time,
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: pack_meta,
        })
    }

    fn drain_source(
        source: &StatamSource,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let streams = source.open_partitions(spec, ctx)?;
        let cancel = ctx.eval.cancel.clone();
        let pool = Arc::clone(&ctx.pool);
        let tasks: Vec<_> = streams
            .into_iter()
            .map(|mut s| {
                let c = cancel.clone();
                let p = Arc::clone(&pool);
                move |_idx| {
                    let mut out = Vec::new();
                    let mut res = p.register("test_drain", false);
                    loop {
                        res.free();
                        match s.next_batch(&c, Some(&mut res))? {
                            Some(b) => out.push(b),
                            None => break,
                        }
                    }
                    res.free();
                    Ok(out)
                }
            })
            .collect();
        let parts = ctx.workers.run_scoped(&cancel, tasks)?;
        Ok(parts.into_iter().flatten().collect())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn map_projections_lattice_respects_full_keys_and_filter_merge() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.metadata.insert("env".to_owned(), "prod".to_owned());
        s1.metadata.insert("team".to_owned(), "core".to_owned());
        s1.metadata.insert("extra".to_owned(), "keep".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.metadata.insert("env".to_owned(), "dev".to_owned());
        s2.metadata.insert("team".to_owned(), "ml".to_owned());
        let batch = span_rows(&[s1, s2]);
        let file = put_statam_file(&store, FileTable::Spans, 1, day(5), &batch).await;

        let schema = FileTable::Spans.column_schema();
        let meta_idx = schema.index_of(spans::METADATA).unwrap();
        let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
        let queue = Arc::new(VerifyQueue::default());
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            cache,
            Suspects::new(queue),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // Case 1: MapProjection::Keys({"env"}) + filter on metadata['team'] = 'core' -> merges both keys!
        let filter = Expr::Binary {
            left: Box::new(Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: meta_idx,
                    name: spans::METADATA.to_owned(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Literal(ScalarValue::Utf8("team".to_owned()))),
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("core".to_owned()))),
            data_type: DataType::Bool,
        };
        let mut mp = BTreeMap::new();
        mp.insert(
            spans::METADATA.to_owned(),
            MapProjection::Keys(BTreeSet::from(["env".to_owned()])),
        );
        let spec = ScanSpec {
            projection: Some(vec![meta_idx]),
            map_projections: mp,
            filters: vec![filter],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec, &ctx).unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 1, "filter on metadata['team'] must find s1");

        // Case 2: MapProjection::KeysOnly must NOT emit `map_keys` (must preserve all keys).
        let mut mp_keys_only = BTreeMap::new();
        mp_keys_only.insert(spans::METADATA.to_owned(), MapProjection::KeysOnly);
        let spec_keys_only = ScanSpec {
            projection: Some(vec![meta_idx]),
            map_projections: mp_keys_only,
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_keys_only, &ctx).unwrap();
        let rows: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(rows, 2);
        // Verify `extra` key is still present on s1 (not pruned away).
        let combined = Batch::concat(
            Arc::new(schema.project(&[meta_idx]).unwrap()),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        let ScalarValue::MapUtf8(entries) = combined.column(0).unwrap().get_scalar(0).unwrap()
        else {
            panic!("expected MapUtf8");
        };
        assert_eq!(entries.len(), 3, "KeysOnly must retain all 3 map keys");

        // Case 3: Projected MapUtf8 absent from `spec.map_projections` + filter on `metadata['env']`:
        // Must treat output column as `Full` (so all 3 keys survive in output!).
        let env_filter = Expr::Binary {
            left: Box::new(Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: meta_idx,
                    name: spans::METADATA.to_owned(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Literal(ScalarValue::Utf8("env".to_owned()))),
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("prod".to_owned()))),
            data_type: DataType::Bool,
        };
        let spec_full = ScanSpec {
            projection: Some(vec![meta_idx]),
            filters: vec![env_filter],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_full, &ctx).unwrap();
        let combined = Batch::concat(
            Arc::new(schema.project(&[meta_idx]).unwrap()),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        assert_eq!(combined.num_rows(), 1);
        let ScalarValue::MapUtf8(entries) = combined.column(0).unwrap().get_scalar(0).unwrap()
        else {
            panic!("expected MapUtf8");
        };
        assert_eq!(
            entries.len(),
            3,
            "absent from map_projections means Full even with map_key filter"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cnf_substrings_push_single_disjunct_and_preserve_or_disjuncts() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.input = Some("alpha ".repeat(200));
        s1.output = Some("none".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.input = Some("none".to_owned());
        s2.output = Some("beta ".repeat(200));
        let mut s3 = span_row(3, 0, ns(5, 30), ns(5, 31));
        s3.input = Some("gamma".to_owned());
        s3.output = Some("delta".to_owned());
        let batch = span_rows(&[s1, s2, s3]);
        let file = put_l1_spans_with_pack(&store, day(5), &batch).await;
        assert!(
            file.pack.is_some(),
            "L1 spans with >1KB payloads creates sibling .stpk"
        );

        let schema = FileTable::Spans.column_schema();
        let in_idx = schema.index_of(spans::INPUT).unwrap();
        let out_idx = schema.index_of(spans::OUTPUT).unwrap();
        let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            Arc::clone(&cache),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // Multi-disjunct OR: (input ILIKE '%alpha%' OR output ILIKE '%beta%').
        // Must return BOTH s1 and s2 (never flattened into AND!).
        let or_expr = Expr::Binary {
            left: Box::new(Expr::Call {
                func: ScalarFn::Contains,
                args: vec![
                    Expr::Column {
                        index: in_idx,
                        name: spans::INPUT.to_owned(),
                        data_type: DataType::Utf8,
                    },
                    Expr::Literal(ScalarValue::Utf8("alpha".to_owned())),
                ],
                data_type: DataType::Bool,
            }),
            op: BinaryOp::Or,
            right: Box::new(Expr::Call {
                func: ScalarFn::Contains,
                args: vec![
                    Expr::Column {
                        index: out_idx,
                        name: spans::OUTPUT.to_owned(),
                        data_type: DataType::Utf8,
                    },
                    Expr::Literal(ScalarValue::Utf8("beta".to_owned())),
                ],
                data_type: DataType::Bool,
            }),
            data_type: DataType::Bool,
        };
        let spec_or = ScanSpec {
            projection: Some(vec![in_idx, out_idx]),
            filters: vec![or_expr],
            substrings: vec![vec![
                SubstringHint {
                    column: spans::INPUT.to_owned(),
                    needle: "alpha".to_owned(),
                    case_insensitive: true,
                },
                SubstringHint {
                    column: spans::OUTPUT.to_owned(),
                    needle: "beta".to_owned(),
                    case_insensitive: true,
                },
            ]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_or, &ctx).unwrap();
        let count: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(count, 2, "OR substring clause must return both s1 and s2");
        assert!(cache.bytes() > 0, "both .statam meta and .stpk toc cached");

        // Second query hits cache for both .statam and .stpk!
        let hits_before = cache.hits();
        let spec_single = ScanSpec {
            projection: Some(vec![in_idx]),
            substrings: vec![vec![SubstringHint {
                column: spans::INPUT.to_owned(),
                needle: "alpha".to_owned(),
                case_insensitive: false,
            }]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_single, &ctx).unwrap();
        let count: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(count, 1);
        assert!(
            cache.hits() >= hits_before + 2,
            "expected StatamFileMeta + PackToc cache hits"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sort_desc_hint_and_around_ns_and_zero_col_count() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let tb1 = trace_batch(&[
            span_row(1, 0, ns(1, 10), ns(1, 11)),
            span_row(2, 0, ns(1, 20), ns(1, 21)),
            span_row(3, 0, ns(1, 30), ns(1, 31)),
        ]);
        let tb2 = trace_batch(&[span_row(4, 0, ns(10, 10), ns(10, 11))]);
        let f1 = put_statam_file(&store, FileTable::Traces, 1, day(1), &tb1).await;
        let f2 = put_statam_file(&store, FileTable::Traces, 1, day(10), &tb2).await;

        let schema = FileTable::Traces.column_schema();
        let start_idx = schema.index_of(traces::START_TIME).unwrap();
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Traces,
            Arc::clone(&schema),
            vec![f1, f2],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 0-column count scan.
        let count_spec = ScanSpec {
            projection: Some(vec![]),
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &count_spec, &ctx).unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 4);
        assert_eq!(batches[0].schema().len(), 0);

        // `around_ns` prunes day(10) when looking up trace 1 around day(1).
        let tid1 = *span_row(1, 0, 0, 0).trace_id.as_bytes();
        let around_spec = ScanSpec {
            projection: Some(vec![start_idx]),
            time_range: TimeRangeHint {
                min_ns: None,
                max_ns: None,
                around_ns: Some(ns(1, 15)),
            },
            id_lookups: vec![IdLookupHint::Fixed16 {
                column: traces::TRACE_ID.to_owned(),
                values: vec![tid1],
            }],
            ..ScanSpec::default()
        };
        let kept = source.prune_files(&around_spec);
        assert_eq!(kept.len(), 1, "around_ns must prune day(10) file");
        let batches = drain_source(&source, &around_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 1);

        // `sort_desc_hint` on Scores with `created_at DESC LIMIT 1`.
        let scores_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let sb = score_rows(&[
            score_row(1, 1, ns(5, 10), ns(5, 10), ns(5, 10), false, "a"),
            score_row(2, 1, ns(5, 20), ns(5, 20), ns(5, 20), false, "b"),
            score_row(3, 1, ns(5, 30), ns(5, 30), ns(5, 30), false, "c"),
        ]);
        let sf = put_statam_file(&scores_store, FileTable::Scores, 1, day(5), &sb).await;
        let sc_schema = FileTable::Scores.column_schema();
        let sc_created = sc_schema.index_of(scores::CREATED_AT).unwrap();
        let sc_source = StatamSource::new(
            StorageIo::from_store(scores_store),
            FileTable::Scores,
            Arc::clone(&sc_schema),
            vec![sf],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let desc_spec = ScanSpec {
            projection: Some(vec![sc_created]),
            sort_hints: vec![SortHint {
                column: scores::CREATED_AT.to_owned(),
                descending: true,
            }],
            limit: Some(1),
            ..ScanSpec::default()
        };
        let batches = drain_source(&sc_source, &desc_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 1);
        let active = batches[0]
            .selection()
            .to_indices(batches[0].raw_len())
            .unwrap();
        let ScalarValue::TimestampNs { value, .. } = batches[0]
            .column(0)
            .unwrap()
            .get_scalar(active[0] as usize)
            .unwrap()
        else {
            panic!("expected TimestampNs");
        };
        assert_eq!(value, ns(5, 30), "sort_desc_hint must return newest row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oom_rollback_cancellation_and_corrupt_suspect_reporting() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&inner)));
        let batch = span_rows(&[
            span_row(1, 0, ns(5, 10), ns(5, 11)),
            span_row(2, 0, ns(5, 20), ns(5, 21)),
        ]);
        let file = put_statam_file(&inner, FileTable::Spans, 0, day(5), &batch).await;
        let schema = FileTable::Spans.column_schema();

        // 1. Reservation OOM rolls back cleanly.
        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&inner)),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let tiny_ctx = make_exec_ctx(16, CancelToken::new());
        let err = drain_source(&source, &ScanSpec::default(), &tiny_ctx).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge), "got {err:?}");
        assert_eq!(
            tiny_ctx.pool.reserved(),
            0,
            "pool must roll back to 0 on OOM"
        );

        // 2. CancelToken mid-slow-S3-GET cancels immediately without hanging.
        chaos.push(Call::Get, Fault::Delay(Duration::from_secs(30)));
        let cancel = CancelToken::new();
        let cancel_source = StatamSource::new(
            StorageIo::from_store(chaos.clone() as Arc<dyn ObjectStore>),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let cancel_ctx = make_exec_ctx(64 * 1024 * 1024, cancel.clone());
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancel_trigger.cancel();
        });
        let started = std::time::Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            drain_source(&cancel_source, &ScanSpec::default(), &cancel_ctx)
        })
        .await
        .unwrap();
        assert!(
            matches!(res, Err(QueryError::Cancelled)),
            "expected Cancelled, got {res:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation must abort delayed GET promptly"
        );

        // 3. Corrupt file reports suspect; transient store error does NOT report suspect.
        let corrupt_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut corrupt_file = (*file).clone();
        corrupt_file.id = FileId(Uuid::now_v7());
        corrupt_store
            .put(
                &ObjectPath::from(corrupt_file.path.as_str()),
                PutPayload::from(Bytes::from_static(b"not a valid statam file at all!!")),
            )
            .await
            .unwrap();
        corrupt_file.size = 32;
        let queue = Arc::new(VerifyQueue::default());
        let bad_source = StatamSource::new(
            StorageIo::from_store(corrupt_store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::new(corrupt_file.clone())],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&queue)),
            tokio::runtime::Handle::current(),
        );
        let ok_ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());
        let err = drain_source(&bad_source, &ScanSpec::default(), &ok_ctx).unwrap_err();
        assert!(matches!(err, QueryError::CorruptData(_)), "got {err:?}");
        assert_eq!(queue.take(10), vec![corrupt_file.id]);

        // Transient store error returns StorageIo and does NOT report suspect.
        let transient_chaos = Arc::new(ChaosStore::new(Arc::clone(&inner)));
        transient_chaos.push(
            Call::Get,
            Fault::Before(crate::test_support::Status::Internal),
        );
        let transient_queue = Arc::new(VerifyQueue::default());
        let transient_source = StatamSource::new(
            StorageIo::from_store(transient_chaos as Arc<dyn ObjectStore>),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&transient_queue)),
            tokio::runtime::Handle::current(),
        );
        let transient_ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());
        let err =
            drain_source(&transient_source, &ScanSpec::default(), &transient_ctx).unwrap_err();
        assert!(
            matches!(err, QueryError::StorageIo(_)),
            "expected StorageIo, got {err:?}"
        );
        assert!(
            transient_queue.take(10).is_empty(),
            "transient store error must not report suspect"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_evolution_fills_missing_nullable_column_with_null() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.name = "first-span".to_owned();
        s1.metadata.insert("env".to_owned(), "prod".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.name = "second-span".to_owned();
        s2.metadata.insert("env".to_owned(), "dev".to_owned());
        let batch = span_rows(&[s1, s2]);
        // File is written with the base spans schema (subset of the evolved schema below).
        let file = put_statam_file(&store, FileTable::Spans, 1, day(5), &batch).await;

        // Evolved schema inserts a new nullable Utf8 column at index 0 (shifting all
        // subsequent column indices) and appends another nullable Utf8 column at the end.
        let mut evolved_fields = vec![cetra_column::Field::new(
            "evolved_prefix",
            DataType::Utf8,
            true,
        )];
        evolved_fields.extend(FileTable::Spans.column_schema().fields().iter().cloned());
        evolved_fields.push(cetra_column::Field::new(
            "evolved_suffix",
            DataType::Utf8,
            true,
        ));
        let evolved_schema = Schema::try_new_arc(evolved_fields).unwrap();
        let prefix_idx = evolved_schema.index_of("evolved_prefix").unwrap();
        let suffix_idx = evolved_schema.index_of("evolved_suffix").unwrap();
        let name_idx = evolved_schema.index_of(spans::NAME).unwrap();
        let meta_idx = evolved_schema.index_of(spans::METADATA).unwrap();

        let queue = Arc::new(VerifyQueue::default());
        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&store)),
            FileTable::Spans,
            Arc::clone(&evolved_schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::clone(&queue)),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 1. Projected missing nullable columns + shifted present column + map_keys projection.
        let mut mp = BTreeMap::new();
        mp.insert(
            spans::METADATA.to_owned(),
            MapProjection::Keys(BTreeSet::from(["env".to_owned()])),
        );
        let proj_spec = ScanSpec {
            projection: Some(vec![prefix_idx, name_idx, meta_idx, suffix_idx]),
            map_projections: mp,
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &proj_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        for b in &batches {
            assert!(matches!(
                b.column(0).unwrap(),
                Column::Const {
                    value: ScalarValue::Null(DataType::Utf8),
                    ..
                }
            ));
            assert!(matches!(
                b.column(3).unwrap(),
                Column::Const {
                    value: ScalarValue::Null(DataType::Utf8),
                    ..
                }
            ));
        }
        let combined = Batch::concat(
            Arc::new(
                evolved_schema
                    .project(&[prefix_idx, name_idx, meta_idx, suffix_idx])
                    .unwrap(),
            ),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        assert_eq!(
            combined.column(1).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Utf8("first-span".to_owned())
        );
        assert_eq!(
            combined.column(1).unwrap().get_scalar(1).unwrap(),
            ScalarValue::Utf8("second-span".to_owned())
        );
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 2. Filter `evolved_suffix IS NULL` returns all rows with Const(Null).
        let is_null_spec = ScanSpec {
            projection: Some(vec![suffix_idx]),
            filters: vec![Expr::Unary {
                op: UnaryOp::IsNull,
                expr: Box::new(Expr::Column {
                    index: suffix_idx,
                    name: "evolved_suffix".to_owned(),
                    data_type: DataType::Utf8,
                }),
                data_type: DataType::Bool,
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &is_null_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        assert!(matches!(
            batches[0].column(0).unwrap(),
            Column::Const {
                value: ScalarValue::Null(DataType::Utf8),
                ..
            }
        ));
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 3. Filter `evolved_suffix = 'x'` short-circuits to 0 rows without error or suspect.
        let eq_spec = ScanSpec {
            projection: Some(vec![name_idx, suffix_idx]),
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: suffix_idx,
                    name: "evolved_suffix".to_owned(),
                    data_type: DataType::Utf8,
                }),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8("x".to_owned()))),
                data_type: DataType::Bool,
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &eq_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 4. Single-disjunct `SubstringHint` on missing nullable column short-circuits to 0 rows
        // (never hits `SchemaMismatch`).
        let sub_spec = ScanSpec {
            projection: Some(vec![name_idx]),
            substrings: vec![vec![SubstringHint {
                column: "evolved_suffix".to_owned(),
                needle: "needle".to_owned(),
                case_insensitive: true,
            }]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &sub_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 5. `IdLookupHint` on missing nullable column short-circuits to 0 rows without error.
        let id_spec = ScanSpec {
            projection: Some(vec![name_idx]),
            id_lookups: vec![IdLookupHint::Utf8 {
                column: "evolved_suffix".to_owned(),
                values: vec!["x".to_owned()],
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &id_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 6. Missing non-nullable column reports corrupt and enqueues suspect.
        let mut bad_fields = FileTable::Spans.column_schema().fields().to_vec();
        bad_fields.push(cetra_column::Field::new(
            "required_missing",
            DataType::Utf8,
            false,
        ));
        let bad_schema = Schema::try_new_arc(bad_fields).unwrap();
        let bad_queue = Arc::new(VerifyQueue::default());
        let bad_source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            bad_schema,
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&bad_queue)),
            tokio::runtime::Handle::current(),
        );
        let err = drain_source(&bad_source, &ScanSpec::default(), &ctx).unwrap_err();
        assert!(matches!(err, QueryError::CorruptData(_)), "got {err:?}");
        assert_eq!(bad_queue.take(10), vec![file.id]);
    }
}
