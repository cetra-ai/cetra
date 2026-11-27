use std::cmp::Ordering as CmpOrdering;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cetra_column::{
    Batch, BinaryData, Bitmap, CancelToken, Column, DataType, DictColumn, EventsData, Field,
    FlatColumn, FlatData, GlobalPool, Interval, LinksData, MapData, RleColumn, ScalarValue, Schema,
    Selection, Utf8Data,
};
use cetra_query::QueryError;
use cetra_query::encode::{format_interval, format_timestamp_ns_fixed9};
use cetra_query::exec::{
    BatchSource, ExecContext, ExecMetrics, SpillCapFn, SpillManager, WorkerPool,
};
use cetra_query::fn_time::format_date_days;
use cetra_query::ir::EvalContext;
use cetra_query::sql::run::prepare_sql_plan_with_limit;
use cetra_query::sql::{
    BoundParams, LogicalPlan, SqlCatalog, SqlSources, bind_statement, compile_plan,
    parse_and_validate_sql, parse_param_values,
};

pub(crate) const FIXED_NOW_NS: i64 = 1_767_225_600_000_000_000;

#[derive(Debug)]
pub(crate) struct CountingExecCtx {
    pub(crate) ctx: ExecContext,
    pub(crate) spill_events: Arc<AtomicU64>,
    pub(crate) _dir: tempfile::TempDir,
}

pub(crate) fn make_exec_ctx(
    memory_cap: usize,
    spill_cap: u64,
    target_partitions: usize,
    batch_size: usize,
) -> CountingExecCtx {
    let dir = tempfile::tempdir().expect("create spill tempdir");
    let spill_events = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&spill_events);
    let cap_fn: SpillCapFn = Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        spill_cap
    });
    let spill = Arc::new(SpillManager::new(dir.path().to_path_buf(), cap_fn));
    let workers = Arc::new(WorkerPool::new(target_partitions.max(1)).expect("create worker pool"));
    let ctx = ExecContext {
        pool: GlobalPool::standalone(memory_cap),
        eval: EvalContext::new(FIXED_NOW_NS, CancelToken::new()),
        spill,
        workers,
        batch_size,
        target_partitions,
        metrics: ExecMetrics::default(),
    };
    CountingExecCtx {
        ctx,
        spill_events,
        _dir: dir,
    }
}

pub(crate) fn assert_clean_ctx(ctx: &ExecContext) {
    assert_eq!(
        ctx.pool.reserved(),
        0,
        "memory pool leaked {} reserved bytes",
        ctx.pool.reserved()
    );
    assert_eq!(
        ctx.spill.spilled_bytes(),
        0,
        "spill manager leaked {} spilled bytes",
        ctx.spill.spilled_bytes()
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutVariant {
    FlatAll,
    MultiChunkFlat(usize),
    FragmentedMasked,
    EncodedMixed,
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        (self.next_u64() % (bound as u64)) as usize
    }
}

fn flat_column_from_scalars(data_type: &DataType, values: &[ScalarValue]) -> FlatColumn {
    let len = values.len();
    if len == 0 {
        return FlatColumn::empty(data_type);
    }

    let mut nulls = Bitmap::new_ones(len);
    for (i, v) in values.iter().enumerate() {
        if v.is_null() {
            nulls.set(i, false).expect("valid bitmap index");
        } else {
            assert_eq!(
                v.data_type(),
                *data_type,
                "scalar type {:?} does not match column type {:?}",
                v.data_type(),
                data_type
            );
        }
    }
    let nulls_opt = if matches!(data_type, DataType::Null) || nulls.all_ones() {
        None
    } else {
        Some(nulls)
    };

    let data = match data_type {
        DataType::Null => FlatData::Null(len),
        DataType::Bool => {
            let mut bm = Bitmap::new_zeroed(len);
            for (i, v) in values.iter().enumerate() {
                if let ScalarValue::Bool(true) = v {
                    bm.set(i, true).expect("valid bool bitmap index");
                }
            }
            FlatData::Bool(bm)
        }
        DataType::I64 => FlatData::I64(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::I64(x) => *x,
                    _ => 0,
                })
                .collect(),
        ),
        DataType::F64 => FlatData::F64(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::F64(x) => *x,
                    _ => 0.0,
                })
                .collect(),
        ),
        DataType::Decimal128 { precision, scale } => FlatData::Decimal128 {
            values: values
                .iter()
                .map(|v| match v {
                    ScalarValue::Decimal128 { value, .. } => *value,
                    _ => 0,
                })
                .collect(),
            precision: *precision,
            scale: *scale,
        },
        DataType::TimestampNs { utc } => FlatData::TimestampNs {
            values: values
                .iter()
                .map(|v| match v {
                    ScalarValue::TimestampNs { value, .. } => *value,
                    _ => 0,
                })
                .collect(),
            utc: *utc,
        },
        DataType::Date => FlatData::Date(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::Date(x) => *x,
                    _ => 0,
                })
                .collect(),
        ),
        DataType::Interval => FlatData::Interval(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::Interval(x) => *x,
                    _ => Interval::default(),
                })
                .collect(),
        ),
        DataType::Utf8 => FlatData::Utf8(
            Utf8Data::try_from_slices(values.iter().map(|v| match v {
                ScalarValue::Utf8(s) => s.as_str(),
                _ => "",
            }))
            .expect("valid utf8 column"),
        ),
        DataType::Binary => FlatData::Binary(
            BinaryData::try_from_slices(values.iter().map(|v| match v {
                ScalarValue::Binary(b) => b.as_slice(),
                _ => &[],
            }))
            .expect("valid binary column"),
        ),
        DataType::FixedBytes8 => FlatData::FixedBytes8(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::FixedBytes8(b) => *b,
                    _ => [0u8; 8],
                })
                .collect(),
        ),
        DataType::FixedBytes16 => FlatData::FixedBytes16(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::FixedBytes16(b) => *b,
                    _ => [0u8; 16],
                })
                .collect(),
        ),
        DataType::List(inner) => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut child_scalars = Vec::new();
            for v in values {
                if let ScalarValue::List { values: items, .. } = v {
                    child_scalars.extend(items.iter().cloned());
                }
                offsets.push(u32::try_from(child_scalars.len()).expect("list offsets fit u32"));
            }
            let child_col = flat_column_from_scalars(inner, &child_scalars);
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(Column::Flat(child_col)),
            }
        }
        DataType::MapUtf8 => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            let mut val_valid = Vec::new();
            for v in values {
                if let ScalarValue::MapUtf8(entries) = v {
                    for (k, mv) in entries {
                        keys.push(k.as_str());
                        vals.push(mv.as_deref().unwrap_or(""));
                        val_valid.push(mv.is_some());
                    }
                }
                offsets.push(u32::try_from(keys.len()).expect("map offsets fit u32"));
            }
            let keys_data = Utf8Data::try_from_slices(keys).expect("valid map keys");
            let vals_data = Utf8Data::try_from_slices(vals).expect("valid map values");
            let val_nulls = Bitmap::from_bools(&val_valid);
            let map = MapData::try_new(offsets, keys_data, vals_data, Some(val_nulls))
                .expect("valid map data");
            FlatData::MapUtf8(Box::new(map))
        }
        DataType::Events => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut times = Vec::new();
            let mut names = Vec::new();
            let mut attrs = Vec::new();
            let mut attr_valid = Vec::new();
            for v in values {
                if let ScalarValue::Events(evs) = v {
                    for ev in evs {
                        times.push(ev.time);
                        names.push(ev.name.as_str());
                        attrs.push(ev.attributes.as_deref().unwrap_or(""));
                        attr_valid.push(ev.attributes.is_some());
                    }
                }
                offsets.push(u32::try_from(times.len()).expect("events offsets fit u32"));
            }
            let names_data = Utf8Data::try_from_slices(names).expect("valid event names");
            let attrs_data = Utf8Data::try_from_slices(attrs).expect("valid event attrs");
            let attr_nulls = Bitmap::from_bools(&attr_valid);
            let events =
                EventsData::try_new(offsets, times, names_data, attrs_data, Some(attr_nulls))
                    .expect("valid events data");
            FlatData::Events(Box::new(events))
        }
        DataType::Links => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut trace_ids = Vec::new();
            let mut span_ids = Vec::new();
            let mut attrs = Vec::new();
            let mut attr_valid = Vec::new();
            for v in values {
                if let ScalarValue::Links(lks) = v {
                    for lk in lks {
                        trace_ids.push(lk.trace_id);
                        span_ids.push(lk.span_id);
                        attrs.push(lk.attributes.as_deref().unwrap_or(""));
                        attr_valid.push(lk.attributes.is_some());
                    }
                }
                offsets.push(u32::try_from(trace_ids.len()).expect("links offsets fit u32"));
            }
            let attrs_data = Utf8Data::try_from_slices(attrs).expect("valid link attrs");
            let attr_nulls = Bitmap::from_bools(&attr_valid);
            let links =
                LinksData::try_new(offsets, trace_ids, span_ids, attrs_data, Some(attr_nulls))
                    .expect("valid links data");
            FlatData::Links(Box::new(links))
        }
    };

    FlatColumn::try_new(data, nulls_opt).expect("valid flat column")
}

pub(crate) fn rows_to_batch(schema: &Arc<Schema>, rows: &[Vec<ScalarValue>]) -> Batch {
    if rows.is_empty() {
        return Batch::empty(Arc::clone(schema));
    }
    for (row_idx, row) in rows.iter().enumerate() {
        assert_eq!(
            row.len(),
            schema.len(),
            "row {row_idx} has {} cells, expected {}",
            row.len(),
            schema.len()
        );
    }
    let columns = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, field)| {
            let col_vals: Vec<ScalarValue> = rows.iter().map(|r| r[col_idx].clone()).collect();
            Column::Flat(flat_column_from_scalars(&field.data_type, &col_vals))
        })
        .collect();
    Batch::try_new_all(Arc::clone(schema), columns, rows.len()).expect("valid flat batch")
}

fn poison_scalar(dt: &DataType, rng_val: u64) -> ScalarValue {
    match dt {
        DataType::Null => ScalarValue::Null(DataType::Null),
        DataType::Bool => ScalarValue::Bool((rng_val & 1) == 0),
        DataType::I64 => {
            let choices = [0i64, i64::MAX, i64::MIN, -1];
            ScalarValue::I64(choices[(rng_val as usize) % choices.len()])
        }
        DataType::F64 => {
            let choices = [0.0f64, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0];
            ScalarValue::F64(choices[(rng_val as usize) % choices.len()])
        }
        DataType::Decimal128 { precision, scale } => {
            let max_mag = 10i128.pow(u32::from(*precision)) - 1;
            let choices = [0i128, max_mag, -max_mag];
            ScalarValue::Decimal128 {
                value: choices[(rng_val as usize) % choices.len()],
                precision: *precision,
                scale: *scale,
            }
        }
        DataType::TimestampNs { utc } => {
            let choices = [0i64, FIXED_NOW_NS, 1_000_000_000, -1_000_000_000];
            ScalarValue::TimestampNs {
                value: choices[(rng_val as usize) % choices.len()],
                utc: *utc,
            }
        }
        DataType::Date => {
            let choices = [0i32, 36_500, -36_500];
            ScalarValue::Date(choices[(rng_val as usize) % choices.len()])
        }
        DataType::Interval => {
            let choices = [
                Interval::default(),
                Interval {
                    months: 12,
                    days: 31,
                    nanos: 1,
                },
                Interval {
                    months: -1,
                    days: -1,
                    nanos: -1,
                },
            ];
            ScalarValue::Interval(choices[(rng_val as usize) % choices.len()])
        }
        DataType::Utf8 => {
            let choices = [
                "__DECOY__",
                "",
                "not-a-number",
                "{invalid-json",
                "[unclosed-regex",
            ];
            ScalarValue::Utf8(choices[(rng_val as usize) % choices.len()].to_owned())
        }
        DataType::Binary => ScalarValue::Binary(vec![0xFF, 0x00, 0xFE]),
        DataType::FixedBytes8 => ScalarValue::FixedBytes8([0xDE; 8]),
        DataType::FixedBytes16 => ScalarValue::FixedBytes16([0xAD; 16]),
        DataType::List(inner) => ScalarValue::List {
            item_type: (**inner).clone(),
            values: vec![poison_scalar(inner, rng_val.wrapping_add(1))],
        },
        DataType::MapUtf8 => ScalarValue::MapUtf8(vec![
            ("__decoy_k__".to_owned(), Some("__DECOY__".to_owned())),
            ("__decoy_null__".to_owned(), None),
        ]),
        DataType::Events => ScalarValue::Events(Vec::new()),
        DataType::Links => ScalarValue::Links(Vec::new()),
    }
}

fn decoy_cell(field: &Field, rng: &mut SplitMix64) -> ScalarValue {
    let r = rng.next_u64();
    if field.nullable && (r & 1) == 0 {
        ScalarValue::Null(field.data_type.clone())
    } else {
        poison_scalar(&field.data_type, r >> 1)
    }
}

fn exact_scalar_eq(a: &ScalarValue, b: &ScalarValue) -> bool {
    match (a, b) {
        (ScalarValue::Null(x), ScalarValue::Null(y)) => x == y,
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => x == y,
        (ScalarValue::I64(x), ScalarValue::I64(y)) => x == y,
        (ScalarValue::F64(x), ScalarValue::F64(y)) => x.to_bits() == y.to_bits(),
        (
            ScalarValue::Decimal128 {
                value: v1,
                precision: p1,
                scale: s1,
            },
            ScalarValue::Decimal128 {
                value: v2,
                precision: p2,
                scale: s2,
            },
        ) => v1 == v2 && p1 == p2 && s1 == s2,
        (
            ScalarValue::TimestampNs { value: v1, utc: u1 },
            ScalarValue::TimestampNs { value: v2, utc: u2 },
        ) => v1 == v2 && u1 == u2,
        (ScalarValue::Date(x), ScalarValue::Date(y)) => x == y,
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => x == y,
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => x == y,
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => x == y,
        (ScalarValue::FixedBytes8(x), ScalarValue::FixedBytes8(y)) => x == y,
        (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => x == y,
        (
            ScalarValue::List {
                item_type: it1,
                values: v1,
            },
            ScalarValue::List {
                item_type: it2,
                values: v2,
            },
        ) => {
            it1 == it2
                && v1.len() == v2.len()
                && v1.iter().zip(v2).all(|(x, y)| exact_scalar_eq(x, y))
        }
        (ScalarValue::MapUtf8(x), ScalarValue::MapUtf8(y)) => x == y,
        (ScalarValue::Events(x), ScalarValue::Events(y)) => x == y,
        (ScalarValue::Links(x), ScalarValue::Links(y)) => x == y,
        _ => false,
    }
}

fn encode_dict_column(dt: &DataType, vals: &[ScalarValue], rng: &mut SplitMix64) -> Column {
    // Prefix dictionary with an unused poison entry and an unused null entry.
    let mut dict_entries = vec![
        poison_scalar(dt, rng.next_u64()),
        ScalarValue::Null(dt.clone()),
    ];
    let mut codes = Vec::with_capacity(vals.len());
    let mut code_valid = Vec::with_capacity(vals.len());

    for v in vals {
        if v.is_null() {
            if (rng.next_u64() & 1) == 0 {
                // Code-level null
                codes.push(0u32);
                code_valid.push(false);
            } else {
                // Value-level null in dictionary slot 1
                codes.push(1u32);
                code_valid.push(true);
            }
        } else {
            let existing = dict_entries[2..]
                .iter()
                .position(|entry| exact_scalar_eq(entry, v))
                .map(|pos| pos + 2);
            let idx = match existing {
                Some(i) => i,
                None => {
                    dict_entries.push(v.clone());
                    dict_entries.len() - 1
                }
            };
            codes.push(u32::try_from(idx).expect("dict code fits u32"));
            code_valid.push(true);
        }
    }
    // Append one trailing unused decoy entry.
    dict_entries.push(poison_scalar(dt, rng.next_u64()));

    let dict_flat = flat_column_from_scalars(dt, &dict_entries);
    let nulls = if code_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&code_valid))
    };
    Column::Dict(DictColumn::try_new(dict_flat, codes, nulls).expect("valid DictColumn"))
}

fn encode_rle_column(dt: &DataType, vals: &[ScalarValue]) -> Column {
    if vals.is_empty() {
        return Column::Rle(
            RleColumn::try_new(FlatColumn::empty(dt), Vec::new()).expect("valid empty RleColumn"),
        );
    }
    let mut run_vals: Vec<ScalarValue> = Vec::new();
    let mut run_ends: Vec<u32> = Vec::new();
    let mut cur = &vals[0];
    for (idx, v) in vals.iter().enumerate().skip(1) {
        if !exact_scalar_eq(cur, v) {
            run_vals.push(cur.clone());
            run_ends.push(u32::try_from(idx).expect("rle end fits u32"));
            cur = v;
        }
    }
    run_vals.push(cur.clone());
    run_ends.push(u32::try_from(vals.len()).expect("rle end fits u32"));
    let flat_vals = flat_column_from_scalars(dt, &run_vals);
    Column::Rle(RleColumn::try_new(flat_vals, run_ends).expect("valid RleColumn"))
}

fn encode_column_mixed(
    dt: &DataType,
    vals: &[ScalarValue],
    encoding_selector: usize,
    rng: &mut SplitMix64,
) -> Column {
    let raw_len = vals.len();
    let is_const = raw_len == 0 || vals.iter().skip(1).all(|v| exact_scalar_eq(&vals[0], v));

    match encoding_selector % 4 {
        0 if is_const => {
            let scalar = if raw_len == 0 {
                poison_scalar(dt, rng.next_u64())
            } else {
                vals[0].clone()
            };
            Column::try_new_const(scalar, raw_len).expect("valid Const column")
        }
        0 | 1 => encode_dict_column(dt, vals, rng),
        2 => encode_rle_column(dt, vals),
        _ => Column::Flat(flat_column_from_scalars(dt, vals)),
    }
}

fn build_empty_encoded_batch(
    schema: &Arc<Schema>,
    chunk_idx: usize,
    rng: &mut SplitMix64,
) -> Batch {
    let cols: Vec<Column> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, field)| {
            // Ensure empty RleColumn (values.len() == 0, run_ends = []) is always exercised.
            match (chunk_idx + col_idx) % 4 {
                0 => Column::Rle(
                    RleColumn::try_new(FlatColumn::empty(&field.data_type), Vec::new())
                        .expect("valid empty RleColumn"),
                ),
                1 => Column::Dict(
                    DictColumn::try_new(FlatColumn::empty(&field.data_type), Vec::new(), None)
                        .expect("valid empty DictColumn"),
                ),
                2 => Column::try_new_const(poison_scalar(&field.data_type, rng.next_u64()), 0)
                    .expect("valid empty Const column"),
                _ => Column::Flat(FlatColumn::empty(&field.data_type)),
            }
        })
        .collect();
    Batch::try_new(Arc::clone(schema), cols, 0, Selection::All).expect("valid 0-row encoded batch")
}

fn build_decoy_only_batch(
    schema: &Arc<Schema>,
    use_indices: bool,
    encode: bool,
    chunk_idx: usize,
    rng: &mut SplitMix64,
) -> Batch {
    let raw_len = 2usize;
    let cols: Vec<Column> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, field)| {
            let vals: Vec<ScalarValue> = (0..raw_len).map(|_| decoy_cell(field, rng)).collect();
            if encode {
                encode_column_mixed(&field.data_type, &vals, chunk_idx + col_idx, rng)
            } else {
                Column::Flat(flat_column_from_scalars(&field.data_type, &vals))
            }
        })
        .collect();
    let sel = if use_indices {
        Selection::Indices(Vec::new())
    } else {
        Selection::Mask(Bitmap::new_zeroed(raw_len))
    };
    Batch::try_new(Arc::clone(schema), cols, raw_len, sel).expect("valid decoy-only batch")
}

fn build_chunk_with_decoys(
    schema: &Arc<Schema>,
    chunk: &[Vec<ScalarValue>],
    use_indices: bool,
    encode: bool,
    chunk_idx: usize,
    rng: &mut SplitMix64,
) -> Batch {
    let active_len = chunk.len();
    let decoy_count = active_len.clamp(1, 4) + rng.next_usize(2);
    let raw_len = active_len + decoy_count;

    // Decide which columns in EncodedMixed can be forced to Const when all active values match.
    let force_const_col: Vec<bool> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, _)| {
            encode
                && (chunk_idx + col_idx).is_multiple_of(4)
                && active_len > 0
                && chunk
                    .iter()
                    .skip(1)
                    .all(|r| exact_scalar_eq(&r[col_idx], &chunk[0][col_idx]))
        })
        .collect();

    let mut phys_rows: Vec<Vec<ScalarValue>> = Vec::with_capacity(raw_len);
    let selection = if !use_indices {
        // Place real rows in ascending physical positions, interleaved with decoys.
        let mut is_active = vec![false; raw_len];
        let mut active_positions: Vec<usize> = (0..raw_len).collect();
        // Fisher-Yates shuffle to pick `active_len` positions, then sort them to keep order.
        for i in (1..active_positions.len()).rev() {
            let j = rng.next_usize(i + 1);
            active_positions.swap(i, j);
        }
        let mut chosen = active_positions[..active_len].to_vec();
        chosen.sort_unstable();
        for pos in &chosen {
            is_active[*pos] = true;
        }
        let mut active_iter = chunk.iter();
        for &active_flag in &is_active {
            if active_flag {
                phys_rows.push(active_iter.next().expect("active row").clone());
            } else {
                let decoy: Vec<ScalarValue> = schema
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(col_idx, field)| {
                        if force_const_col[col_idx] {
                            chunk[0][col_idx].clone()
                        } else {
                            decoy_cell(field, rng)
                        }
                    })
                    .collect();
                phys_rows.push(decoy);
            }
        }
        Selection::Mask(Bitmap::from_bools(&is_active))
    } else {
        // Permute physical slots arbitrarily and map active logical rows via Selection::Indices.
        let mut slots: Vec<Option<usize>> = (0..active_len).map(Some).collect();
        slots.resize(raw_len, None);
        for i in (1..slots.len()).rev() {
            let j = rng.next_usize(i + 1);
            slots.swap(i, j);
        }
        let mut active_indices = vec![0u32; active_len];
        for (phys_idx, slot) in slots.iter().enumerate() {
            match slot {
                Some(logical_idx) => {
                    active_indices[*logical_idx] =
                        u32::try_from(phys_idx).expect("physical index fits u32");
                    phys_rows.push(chunk[*logical_idx].clone());
                }
                None => {
                    let decoy: Vec<ScalarValue> = schema
                        .fields()
                        .iter()
                        .enumerate()
                        .map(|(col_idx, field)| {
                            if force_const_col[col_idx] {
                                chunk[0][col_idx].clone()
                            } else {
                                decoy_cell(field, rng)
                            }
                        })
                        .collect();
                    phys_rows.push(decoy);
                }
            }
        }
        Selection::Indices(active_indices)
    };

    let cols: Vec<Column> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, field)| {
            let col_vals: Vec<ScalarValue> = phys_rows.iter().map(|r| r[col_idx].clone()).collect();
            if encode {
                encode_column_mixed(&field.data_type, &col_vals, chunk_idx + col_idx, rng)
            } else {
                Column::Flat(flat_column_from_scalars(&field.data_type, &col_vals))
            }
        })
        .collect();

    Batch::try_new(Arc::clone(schema), cols, raw_len, selection)
        .expect("valid masked/indexed batch")
}

pub(crate) fn build_batch_source(
    schema: &Arc<Schema>,
    rows: &[Vec<ScalarValue>],
    variant: LayoutVariant,
    seed: u64,
) -> Arc<BatchSource> {
    let mut rng = SplitMix64::new(seed);
    let batches = match variant {
        LayoutVariant::FlatAll => vec![rows_to_batch(schema, rows)],
        LayoutVariant::MultiChunkFlat(min_chunks) => {
            if rows.is_empty() {
                vec![Batch::empty(Arc::clone(schema))]
            } else {
                let n_chunks = min_chunks.min(rows.len()).max(1);
                let base = rows.len() / n_chunks;
                let rem = rows.len() % n_chunks;
                let mut out = Vec::with_capacity(n_chunks);
                let mut offset = 0usize;
                for i in 0..n_chunks {
                    let sz = base + usize::from(i < rem);
                    out.push(rows_to_batch(schema, &rows[offset..offset + sz]));
                    offset += sz;
                }
                out
            }
        }
        LayoutVariant::FragmentedMasked => {
            let mut out = vec![Batch::empty(Arc::clone(schema))];
            if rows.is_empty() {
                out.push(build_decoy_only_batch(schema, false, false, 0, &mut rng));
                out.push(build_decoy_only_batch(schema, true, false, 1, &mut rng));
            } else {
                let sizes = [1usize, 2, 3, 5, 7];
                let mut offset = 0usize;
                let mut chunk_idx = 0usize;
                while offset < rows.len() {
                    let sz = sizes[chunk_idx % sizes.len()].min(rows.len() - offset);
                    let use_indices = chunk_idx % 2 == 1;
                    out.push(build_chunk_with_decoys(
                        schema,
                        &rows[offset..offset + sz],
                        use_indices,
                        false,
                        chunk_idx,
                        &mut rng,
                    ));
                    if chunk_idx.is_multiple_of(3) {
                        out.push(Batch::empty(Arc::clone(schema)));
                    } else if chunk_idx % 3 == 1 {
                        out.push(build_decoy_only_batch(
                            schema,
                            !use_indices,
                            false,
                            chunk_idx,
                            &mut rng,
                        ));
                    }
                    offset += sz;
                    chunk_idx += 1;
                }
            }
            out
        }
        LayoutVariant::EncodedMixed => {
            // Always include a 0-row encoded batch (empty RleColumn has values.len() == 0, run_ends = []).
            let mut out = vec![build_empty_encoded_batch(schema, 0, &mut rng)];
            if rows.is_empty() {
                out.push(build_decoy_only_batch(schema, false, true, 1, &mut rng));
                out.push(build_decoy_only_batch(schema, true, true, 2, &mut rng));
            } else {
                let n_chunks = if rows.len() >= 8 {
                    (rows.len() / 4).max(8)
                } else {
                    rows.len()
                };
                let base = rows.len() / n_chunks;
                let rem = rows.len() % n_chunks;
                let mut offset = 0usize;
                for chunk_idx in 0..n_chunks {
                    let sz = base + usize::from(chunk_idx < rem);
                    let use_indices = chunk_idx % 2 == 1;
                    out.push(build_chunk_with_decoys(
                        schema,
                        &rows[offset..offset + sz],
                        use_indices,
                        true,
                        chunk_idx,
                        &mut rng,
                    ));
                    if chunk_idx == n_chunks / 2 {
                        out.push(build_decoy_only_batch(
                            schema,
                            !use_indices,
                            true,
                            chunk_idx + 1,
                            &mut rng,
                        ));
                    }
                    offset += sz;
                }
            }
            out
        }
    };
    Arc::new(BatchSource::new(Arc::clone(schema), batches).expect("valid BatchSource"))
}

fn extract_batch_rows(batch: &Batch, out: &mut Vec<Vec<ScalarValue>>) -> Result<(), QueryError> {
    if batch.is_empty() {
        return Ok(());
    }
    let cols = batch.columns();
    match batch.selection() {
        Selection::All => {
            for phys in 0..batch.raw_len() {
                let mut row = Vec::with_capacity(cols.len());
                for col in cols {
                    row.push(col.get_scalar(phys)?);
                }
                out.push(row);
            }
        }
        Selection::Mask(bm) => {
            for phys in bm.iter_ones() {
                let mut row = Vec::with_capacity(cols.len());
                for col in cols {
                    row.push(col.get_scalar(phys)?);
                }
                out.push(row);
            }
        }
        Selection::Indices(indices) => {
            for &phys in indices {
                let mut row = Vec::with_capacity(cols.len());
                for col in cols {
                    row.push(col.get_scalar(phys as usize)?);
                }
                out.push(row);
            }
        }
    }
    Ok(())
}

pub(crate) fn collect_plan_rows(
    plan: &LogicalPlan,
    sources: &SqlSources,
    ctx: &ExecContext,
) -> Result<(Arc<Schema>, Vec<Vec<ScalarValue>>), QueryError> {
    let schema = Arc::clone(plan.schema());
    let mut stream = match compile_plan(plan, sources, ctx) {
        Ok(s) => s,
        Err(err) => {
            assert_clean_ctx(ctx);
            return Err(err);
        }
    };
    let mut pull_res = ctx.pool.register("verify_collect", false);
    let mut rows = Vec::new();
    let result = loop {
        match stream.next_batch(&ctx.eval.cancel, Some(&mut pull_res)) {
            Ok(Some(batch)) => {
                let extracted = extract_batch_rows(&batch, &mut rows);
                drop(batch);
                pull_res.free();
                if let Err(err) = extracted {
                    break Err(err);
                }
            }
            Ok(None) => {
                pull_res.free();
                break Ok(());
            }
            Err(err) => {
                pull_res.free();
                break Err(err);
            }
        }
    };
    drop(stream);
    drop(pull_res);
    assert_clean_ctx(ctx);
    result.map(|()| (schema, rows))
}

fn fresh_query_ctx(ctx: &ExecContext) -> ExecContext {
    ExecContext {
        pool: Arc::clone(&ctx.pool),
        eval: EvalContext::new(ctx.eval.now_ns, CancelToken::new()),
        spill: Arc::clone(&ctx.spill),
        workers: Arc::clone(&ctx.workers),
        batch_size: ctx.batch_size,
        target_partitions: ctx.target_partitions,
        metrics: ctx.metrics.clone(),
    }
}

pub(crate) fn run_sql_unoptimized(
    catalog: &SqlCatalog,
    sources: &SqlSources,
    ctx: &ExecContext,
    sql: &str,
    limit: Option<usize>,
) -> Result<(Arc<Schema>, Vec<Vec<ScalarValue>>), QueryError> {
    let query_ctx = fresh_query_ctx(ctx);
    let (stmt, placeholders) = match parse_and_validate_sql(sql, &catalog.registered_names()) {
        Ok(parsed) => parsed,
        Err(err) => {
            let qe: QueryError = err.into();
            assert!(
                !matches!(qe, QueryError::Internal(_)),
                "unexpected Internal error in unoptimized parse/validate: {qe}"
            );
            assert_clean_ctx(&query_ctx);
            return Err(qe);
        }
    };
    let param_values = match parse_param_values(&placeholders, &serde_json::Map::new()) {
        Ok(vals) => vals,
        Err(err) => {
            assert!(
                !matches!(err, QueryError::Internal(_)),
                "unexpected Internal error in unoptimized params: {err}"
            );
            assert_clean_ctx(&query_ctx);
            return Err(err);
        }
    };
    let bound = BoundParams::Values(&param_values);
    let plan = match bind_statement(&stmt, catalog, &bound) {
        Ok(plan) => plan,
        Err(err) => {
            assert!(
                !matches!(err, QueryError::Internal(_)),
                "unexpected Internal error in unoptimized bind: {err}"
            );
            assert_clean_ctx(&query_ctx);
            return Err(err);
        }
    };
    let plan = match (limit, plan) {
        (Some(fetch), p) if !matches!(p, LogicalPlan::Explain { .. }) => LogicalPlan::Limit {
            input: Box::new(p),
            skip: 0,
            fetch: Some(fetch),
        },
        (_, p) => p,
    };
    let res = collect_plan_rows(&plan, sources, &query_ctx);
    if let Err(ref err) = res {
        assert!(
            !matches!(err, QueryError::Internal(_)),
            "unexpected Internal error in unoptimized exec: {err}"
        );
    }
    res
}

pub(crate) fn run_sql_optimized(
    catalog: &SqlCatalog,
    sources: &SqlSources,
    ctx: &ExecContext,
    sql: &str,
    limit: Option<usize>,
) -> Result<(Arc<Schema>, Vec<Vec<ScalarValue>>), QueryError> {
    let query_ctx = fresh_query_ctx(ctx);
    let plan = match prepare_sql_plan_with_limit(catalog, sql, &serde_json::Map::new(), limit) {
        Ok(plan) => plan,
        Err(err) => {
            assert!(
                !matches!(err, QueryError::Internal(_)),
                "unexpected Internal error in optimized prepare: {err}"
            );
            assert_clean_ctx(&query_ctx);
            return Err(err);
        }
    };
    let res = collect_plan_rows(&plan, sources, &query_ctx);
    if let Err(ref err) = res {
        assert!(
            !matches!(err, QueryError::Internal(_)),
            "unexpected Internal error in optimized exec: {err}"
        );
    }
    res
}

#[derive(Debug, Clone)]
pub(crate) struct CompareOpts {
    pub(crate) ordered: bool,
    pub(crate) unordered_arrays: bool,
    pub(crate) float_rel_tol: f64,
    pub(crate) float_abs_tol: f64,
}

impl Default for CompareOpts {
    fn default() -> Self {
        Self {
            ordered: false,
            unordered_arrays: false,
            float_rel_tol: 1e-9,
            float_abs_tol: 1e-12,
        }
    }
}

fn format_decimal(value: i128, scale: i8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    if scale < 0 {
        if value == 0 {
            return "0".to_owned();
        }
        let mut s = value.to_string();
        for _ in 0..usize::from(scale.unsigned_abs()) {
            s.push('0');
        }
        return s;
    }
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let sc = scale as usize;
    let mut out = String::with_capacity(digits.len() + sc + 3);
    if neg {
        out.push('-');
    }
    if digits.len() <= sc {
        out.push_str("0.");
        for _ in 0..(sc - digits.len()) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let split = digits.len() - sc;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    }
    out
}

fn normalize_decimal_canonical(value: i128, scale: i8) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let s = format_decimal(value, scale);
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        if trimmed == "-0" || trimmed.is_empty() {
            "0".to_owned()
        } else {
            trimmed.to_owned()
        }
    } else {
        s
    }
}

pub(crate) fn format_scalar_for_slt(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Null(_) => "NULL".to_owned(),
        ScalarValue::Bool(b) => {
            if *b {
                "true".to_owned()
            } else {
                "false".to_owned()
            }
        }
        ScalarValue::I64(n) => n.to_string(),
        ScalarValue::F64(x) => {
            if x.is_nan() {
                "NaN".to_owned()
            } else if x.is_infinite() {
                if x.is_sign_positive() {
                    "Infinity".to_owned()
                } else {
                    "-Infinity".to_owned()
                }
            } else if *x == 0.0 {
                "0".to_owned()
            } else {
                format!("{x}")
            }
        }
        ScalarValue::Decimal128 { value, scale, .. } => format_decimal(*value, *scale),
        ScalarValue::TimestampNs { value, utc } => format_timestamp_ns_fixed9(*value, *utc),
        ScalarValue::Date(days) => format_date_days(*days),
        ScalarValue::Interval(iv) => format_interval(*iv),
        ScalarValue::Utf8(s) => {
            if s.is_empty() {
                "(empty)".to_owned()
            } else {
                s.clone()
            }
        }
        ScalarValue::Binary(b) => {
            if b.is_empty() {
                "(empty)".to_owned()
            } else {
                hex::encode(b)
            }
        }
        ScalarValue::FixedBytes8(b) => hex::encode(b),
        ScalarValue::FixedBytes16(b) => hex::encode(b),
        ScalarValue::List { values, .. } => {
            let parts: Vec<String> = values.iter().map(format_scalar_for_slt).collect();
            format!("[{}]", parts.join(", "))
        }
        ScalarValue::MapUtf8(entries) => {
            let mut sorted: Vec<(&str, Option<&str>)> = entries
                .iter()
                .map(|(k, val)| (k.as_str(), val.as_deref()))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
            let parts: Vec<String> = sorted
                .into_iter()
                .map(|(k, val)| match val {
                    Some(s) => format!("{k}={s}"),
                    None => format!("{k}=NULL"),
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        ScalarValue::Events(evs) => {
            let mut out = String::from("[");
            for (idx, ev) in evs.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                let _ = write!(
                    out,
                    "({},{},{})",
                    format_timestamp_ns_fixed9(ev.time, true),
                    ev.name,
                    ev.attributes.as_deref().unwrap_or("NULL")
                );
            }
            out.push(']');
            out
        }
        ScalarValue::Links(lks) => {
            let mut out = String::from("[");
            for (idx, lk) in lks.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                let _ = write!(
                    out,
                    "({},{},{})",
                    hex::encode(lk.trace_id),
                    hex::encode(lk.span_id),
                    lk.attributes.as_deref().unwrap_or("NULL")
                );
            }
            out.push(']');
            out
        }
    }
}

pub(crate) fn canonical_scalar_key(v: &ScalarValue, unordered_arrays: bool) -> String {
    match v {
        ScalarValue::Null(_) => "0:NULL".to_owned(),
        ScalarValue::Bool(b) => {
            if *b {
                "1:1".to_owned()
            } else {
                "1:0".to_owned()
            }
        }
        ScalarValue::I64(n) => {
            let shifted = i128::from(*n) - i128::from(i64::MIN);
            format!("2:{shifted:020}")
        }
        ScalarValue::F64(x) => {
            if x.is_nan() {
                "3:NaN".to_owned()
            } else if x.is_infinite() {
                if x.is_sign_positive() {
                    "3:+Inf".to_owned()
                } else {
                    "3:-Inf".to_owned()
                }
            } else {
                let norm = if x.abs() < 1e-12 { 0.0 } else { *x };
                format!("3:{norm:+.6e}")
            }
        }
        ScalarValue::Decimal128 { value, scale, .. } => {
            format!("4:{}", normalize_decimal_canonical(*value, *scale))
        }
        ScalarValue::TimestampNs { value, utc } => {
            format!("5:{}:{}", if *utc { "Z" } else { "N" }, value)
        }
        ScalarValue::Date(days) => format!("6:{days}"),
        ScalarValue::Interval(iv) => format!("7:{}", iv.canonical_key()),
        ScalarValue::Utf8(s) => format!("8:{}:{s}", s.len()),
        ScalarValue::Binary(b) => format!("9:{}", hex::encode(b)),
        ScalarValue::FixedBytes8(b) => format!("a:{}", hex::encode(b)),
        ScalarValue::FixedBytes16(b) => format!("b:{}", hex::encode(b)),
        ScalarValue::List { values, .. } => {
            let mut elem_keys: Vec<String> = values
                .iter()
                .map(|e| canonical_scalar_key(e, unordered_arrays))
                .collect();
            if unordered_arrays {
                elem_keys.sort();
            }
            let mut out = format!("c:{}:[", elem_keys.len());
            for (idx, k) in elem_keys.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}:{k}", k.len());
            }
            out.push(']');
            out
        }
        ScalarValue::MapUtf8(entries) => {
            let mut sorted: Vec<(&str, Option<&str>)> = entries
                .iter()
                .map(|(k, val)| (k.as_str(), val.as_deref()))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
            let mut out = format!("d:{}:{{", sorted.len());
            for (idx, (k, val)) in sorted.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                match val {
                    Some(s) => {
                        let _ = write!(out, "{}:{k}=S:{}:{s}", k.len(), s.len());
                    }
                    None => {
                        let _ = write!(out, "{}:{k}=N", k.len());
                    }
                }
            }
            out.push('}');
            out
        }
        ScalarValue::Events(evs) => {
            let mut out = format!("e:{}:[", evs.len());
            for (idx, ev) in evs.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "{}:{}:{}:{:?}",
                    ev.time,
                    ev.name.len(),
                    ev.name,
                    ev.attributes
                );
            }
            out.push(']');
            out
        }
        ScalarValue::Links(lks) => {
            let mut out = format!("f:{}:[", lks.len());
            for (idx, lk) in lks.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "{}:{}:{:?}",
                    hex::encode(lk.trace_id),
                    hex::encode(lk.span_id),
                    lk.attributes
                );
            }
            out.push(']');
            out
        }
    }
}

fn scalar_tag(v: &ScalarValue) -> u8 {
    match v {
        ScalarValue::Null(_) => 0,
        ScalarValue::Bool(_) => 1,
        ScalarValue::I64(_) => 2,
        ScalarValue::F64(_) => 3,
        ScalarValue::Decimal128 { .. } => 4,
        ScalarValue::TimestampNs { .. } => 5,
        ScalarValue::Date(_) => 6,
        ScalarValue::Interval(_) => 7,
        ScalarValue::Utf8(_) => 8,
        ScalarValue::Binary(_) => 9,
        ScalarValue::FixedBytes8(_) => 10,
        ScalarValue::FixedBytes16(_) => 11,
        ScalarValue::List { .. } => 12,
        ScalarValue::MapUtf8(_) => 13,
        ScalarValue::Events(_) => 14,
        ScalarValue::Links(_) => 15,
    }
}

fn cmp_scalar_exact(a: &ScalarValue, b: &ScalarValue, unordered_arrays: bool) -> CmpOrdering {
    let tag_cmp = scalar_tag(a).cmp(&scalar_tag(b));
    if tag_cmp != CmpOrdering::Equal {
        return tag_cmp;
    }
    match (a, b) {
        (ScalarValue::Null(_), ScalarValue::Null(_)) => CmpOrdering::Equal,
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => x.cmp(y),
        (ScalarValue::I64(x), ScalarValue::I64(y)) => x.cmp(y),
        (ScalarValue::F64(x), ScalarValue::F64(y)) => {
            let nx = if *x == 0.0 { 0.0 } else { *x };
            let ny = if *y == 0.0 { 0.0 } else { *y };
            nx.total_cmp(&ny)
        }
        (
            ScalarValue::Decimal128 {
                value: v1,
                scale: s1,
                ..
            },
            ScalarValue::Decimal128 {
                value: v2,
                scale: s2,
                ..
            },
        ) => normalize_decimal_canonical(*v1, *s1).cmp(&normalize_decimal_canonical(*v2, *s2)),
        (
            ScalarValue::TimestampNs { value: v1, utc: u1 },
            ScalarValue::TimestampNs { value: v2, utc: u2 },
        ) => u1.cmp(u2).then_with(|| v1.cmp(v2)),
        (ScalarValue::Date(x), ScalarValue::Date(y)) => x.cmp(y),
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => {
            x.canonical_key().cmp(&y.canonical_key())
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => x.cmp(y),
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => x.cmp(y),
        (ScalarValue::FixedBytes8(x), ScalarValue::FixedBytes8(y)) => x.cmp(y),
        (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => x.cmp(y),
        (ScalarValue::List { values: x, .. }, ScalarValue::List { values: y, .. }) => {
            if unordered_arrays {
                let mut sx: Vec<usize> = (0..x.len()).collect();
                let mut sy: Vec<usize> = (0..y.len()).collect();
                sx.sort_by(|&i, &j| cmp_scalar_exact(&x[i], &x[j], true));
                sy.sort_by(|&i, &j| cmp_scalar_exact(&y[i], &y[j], true));
                for (&i, &j) in sx.iter().zip(&sy) {
                    let c = cmp_scalar_exact(&x[i], &y[j], true);
                    if c != CmpOrdering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            } else {
                for (xi, yi) in x.iter().zip(y) {
                    let c = cmp_scalar_exact(xi, yi, false);
                    if c != CmpOrdering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
        }
        _ => canonical_scalar_key(a, unordered_arrays)
            .cmp(&canonical_scalar_key(b, unordered_arrays)),
    }
}

fn scalars_equiv(a: &ScalarValue, b: &ScalarValue, opts: &CompareOpts) -> bool {
    match (a, b) {
        (ScalarValue::Null(_), ScalarValue::Null(_)) => true,
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => x == y,
        (ScalarValue::I64(x), ScalarValue::I64(y)) => x == y,
        (ScalarValue::F64(x), ScalarValue::F64(y)) => {
            if x.is_nan() || y.is_nan() {
                return x.is_nan() && y.is_nan();
            }
            if x.is_infinite() || y.is_infinite() {
                return x.is_infinite()
                    && y.is_infinite()
                    && x.is_sign_positive() == y.is_sign_positive();
            }
            let diff = (x - y).abs();
            let tol = opts
                .float_abs_tol
                .max(opts.float_rel_tol * x.abs().max(y.abs()));
            diff <= tol
        }
        (
            ScalarValue::Decimal128 {
                value: v1,
                scale: s1,
                ..
            },
            ScalarValue::Decimal128 {
                value: v2,
                scale: s2,
                ..
            },
        ) => normalize_decimal_canonical(*v1, *s1) == normalize_decimal_canonical(*v2, *s2),
        (
            ScalarValue::TimestampNs { value: v1, utc: u1 },
            ScalarValue::TimestampNs { value: v2, utc: u2 },
        ) => v1 == v2 && u1 == u2,
        (ScalarValue::Date(x), ScalarValue::Date(y)) => x == y,
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => {
            x.canonical_key() == y.canonical_key()
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => x == y,
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => x == y,
        (ScalarValue::FixedBytes8(x), ScalarValue::FixedBytes8(y)) => x == y,
        (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => x == y,
        (ScalarValue::List { values: x, .. }, ScalarValue::List { values: y, .. }) => {
            if x.len() != y.len() {
                return false;
            }
            if !opts.unordered_arrays {
                return x.iter().zip(y).all(|(xi, yi)| scalars_equiv(xi, yi, opts));
            }
            let mut sx: Vec<usize> = (0..x.len()).collect();
            let mut sy: Vec<usize> = (0..y.len()).collect();
            sx.sort_by_cached_key(|&i| canonical_scalar_key(&x[i], true));
            sy.sort_by_cached_key(|&i| canonical_scalar_key(&y[i], true));
            if sx
                .iter()
                .zip(&sy)
                .all(|(&i, &j)| scalars_equiv(&x[i], &y[j], opts))
            {
                return true;
            }
            sx.sort_by(|&i, &j| cmp_scalar_exact(&x[i], &x[j], true));
            sy.sort_by(|&i, &j| cmp_scalar_exact(&y[i], &y[j], true));
            sx.iter()
                .zip(&sy)
                .all(|(&i, &j)| scalars_equiv(&x[i], &y[j], opts))
        }
        (ScalarValue::MapUtf8(x), ScalarValue::MapUtf8(y)) => {
            if x.len() != y.len() {
                return false;
            }
            let mut sx: Vec<(&str, Option<&str>)> =
                x.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
            let mut sy: Vec<(&str, Option<&str>)> =
                y.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
            sx.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
            sy.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
            sx == sy
        }
        (ScalarValue::Events(x), ScalarValue::Events(y)) => x == y,
        (ScalarValue::Links(x), ScalarValue::Links(y)) => x == y,
        _ => false,
    }
}

fn row_equiv(a: &[ScalarValue], b: &[ScalarValue], opts: &CompareOpts) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| scalars_equiv(x, y, opts))
}

fn format_row_preview(row: &[ScalarValue]) -> String {
    let cells: Vec<String> = row.iter().map(format_scalar_for_slt).collect();
    cells.join("\t")
}

fn format_rows_preview(rows: &[Vec<ScalarValue>], max_rows: usize) -> String {
    let mut out = String::new();
    for (idx, row) in rows.iter().take(max_rows).enumerate() {
        let _ = writeln!(out, "  [{idx}] {}", format_row_preview(row));
    }
    if rows.len() > max_rows {
        let _ = writeln!(out, "  ... ({} more rows)", rows.len() - max_rows);
    }
    out
}

pub(crate) fn assert_rows_equiv(
    lhs: &[Vec<ScalarValue>],
    rhs: &[Vec<ScalarValue>],
    opts: &CompareOpts,
    label: &str,
) {
    assert_eq!(
        lhs.len(),
        rhs.len(),
        "{label}: row count mismatch (lhs={}, rhs={})\nlhs:\n{}\nrhs:\n{}",
        lhs.len(),
        rhs.len(),
        format_rows_preview(lhs, 10),
        format_rows_preview(rhs, 10)
    );

    if opts.ordered {
        for (row_idx, (l_row, r_row)) in lhs.iter().zip(rhs).enumerate() {
            assert!(
                row_equiv(l_row, r_row, opts),
                "{label}: ordered row mismatch at row {row_idx}\n  lhs: {}\n  rhs: {}",
                format_row_preview(l_row),
                format_row_preview(r_row)
            );
        }
        return;
    }

    let mut l_idx: Vec<usize> = (0..lhs.len()).collect();
    let mut r_idx: Vec<usize> = (0..rhs.len()).collect();
    let row_key = |row: &[ScalarValue]| -> Vec<String> {
        row.iter()
            .map(|c| canonical_scalar_key(c, opts.unordered_arrays))
            .collect()
    };
    l_idx.sort_by_cached_key(|&i| row_key(&lhs[i]));
    r_idx.sort_by_cached_key(|&i| row_key(&rhs[i]));

    if l_idx
        .iter()
        .zip(&r_idx)
        .all(|(&li, &ri)| row_equiv(&lhs[li], &rhs[ri], opts))
    {
        return;
    }

    // Fallback: sort by exact numeric/total comparator in case a float straddled a rounding bucket.
    let cmp_rows = |a: &[ScalarValue], b: &[ScalarValue]| -> CmpOrdering {
        for (ca, cb) in a.iter().zip(b) {
            let c = cmp_scalar_exact(ca, cb, opts.unordered_arrays);
            if c != CmpOrdering::Equal {
                return c;
            }
        }
        a.len().cmp(&b.len())
    };
    l_idx.sort_by(|&i, &j| cmp_rows(&lhs[i], &lhs[j]));
    r_idx.sort_by(|&i, &j| cmp_rows(&rhs[i], &rhs[j]));

    for (pos, (&li, &ri)) in l_idx.iter().zip(&r_idx).enumerate() {
        assert!(
            row_equiv(&lhs[li], &rhs[ri], opts),
            "{label}: unordered row mismatch at sorted position {pos}\n  lhs: {}\n  rhs: {}",
            format_row_preview(&lhs[li]),
            format_row_preview(&rhs[ri])
        );
    }
}

pub(crate) type TableData = (String, Arc<Schema>, Vec<Vec<ScalarValue>>);

pub(crate) fn make_table_catalog_and_sources(
    tables: &[TableData],
    variant: LayoutVariant,
    seed: u64,
) -> (SqlCatalog, SqlSources) {
    let mut catalog = SqlCatalog::new(None, None);
    for (idx, (name, schema, rows)) in tables.iter().enumerate() {
        let table_seed = seed.wrapping_add((idx as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let src = build_batch_source(schema, rows, variant, table_seed);
        catalog
            .register_batch_source(name.as_str(), src)
            .unwrap_or_else(|e| panic!("failed to register table `{name}`: {e}"));
    }
    let mut sources = SqlSources::new();
    sources.register_catalog_batches(&catalog);
    (catalog, sources)
}
