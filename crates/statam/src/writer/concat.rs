//! Column concatenation and slice size estimation helpers for Row Group buffering.

use std::borrow::Cow;

use cetra_column::{BinaryData, Column, DataType, FlatColumn, FlatData, ScalarValue, Utf8Data};

use crate::StatamError;

use super::rg_encode::as_flat_cow;

fn scalar_byte_len(v: &ScalarValue) -> usize {
    match v {
        ScalarValue::Null(_) | ScalarValue::Bool(_) => 1,
        ScalarValue::Date(_) => 4,
        ScalarValue::I64(_)
        | ScalarValue::F64(_)
        | ScalarValue::TimestampNs { .. }
        | ScalarValue::FixedBytes8(_) => 8,
        ScalarValue::Decimal128 { .. }
        | ScalarValue::Interval(_)
        | ScalarValue::FixedBytes16(_) => 16,
        ScalarValue::Utf8(s) => s.len().saturating_add(4),
        ScalarValue::Binary(b) => b.len().saturating_add(4),
        ScalarValue::List { values, .. } => values
            .iter()
            .map(scalar_byte_len)
            .fold(4usize, usize::saturating_add),
        ScalarValue::MapUtf8(entries) => entries.iter().fold(4usize, |acc, (k, val)| {
            acc.saturating_add(k.len())
                .saturating_add(val.as_ref().map_or(0, String::len))
                .saturating_add(8)
        }),
        ScalarValue::Events(events) => events.iter().fold(4usize, |acc, e| {
            acc.saturating_add(8)
                .saturating_add(e.name.len())
                .saturating_add(e.attributes.as_ref().map_or(0, String::len))
        }),
        ScalarValue::Links(links) => links.iter().fold(4usize, |acc, l| {
            acc.saturating_add(24)
                .saturating_add(l.attributes.as_ref().map_or(0, String::len))
        }),
    }
}

fn estimate_flat_column_bytes(f: &FlatColumn) -> usize {
    let null_bytes = f.nulls().map_or(0, |bm| bm.len().div_ceil(8));
    let data_bytes = match f.data() {
        FlatData::Null(_) => 0,
        FlatData::Bool(b) => b.len().div_ceil(8),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => v.len().saturating_mul(8),
        FlatData::F64(v) => v.len().saturating_mul(8),
        FlatData::Date(v) => v.len().saturating_mul(4),
        FlatData::Decimal128 { values: v, .. } => v.len().saturating_mul(16),
        FlatData::Interval(v) => v.len().saturating_mul(16),
        FlatData::FixedBytes8(v) => v.len().saturating_mul(8),
        FlatData::FixedBytes16(v) => v.len().saturating_mul(16),
        FlatData::Utf8(u) => u.bytes().len().saturating_add(u.len().saturating_mul(4)),
        FlatData::Binary(b) => b.bytes().len().saturating_add(b.len().saturating_mul(4)),
        FlatData::List {
            offsets, values, ..
        } => offsets
            .len()
            .saturating_mul(4)
            .saturating_add(estimate_column_bytes(values)),
        FlatData::MapUtf8(m) => m
            .offsets()
            .len()
            .saturating_mul(4)
            .saturating_add(m.keys().bytes().len())
            .saturating_add(m.values().bytes().len())
            .saturating_add(m.entry_count().saturating_mul(8)),
        FlatData::Events(e) => e
            .offsets()
            .len()
            .saturating_mul(4)
            .saturating_add(e.event_count().saturating_mul(8))
            .saturating_add(e.names().bytes().len())
            .saturating_add(e.attributes().bytes().len()),
        FlatData::Links(l) => l
            .offsets()
            .len()
            .saturating_mul(4)
            .saturating_add(l.link_count().saturating_mul(24))
            .saturating_add(l.attributes().bytes().len()),
    };
    data_bytes.saturating_add(null_bytes)
}

fn estimate_flat_slice_bytes(f: &FlatColumn, offset: usize, len: usize) -> usize {
    estimate_flat_slice_bytes_inner(f, offset, len, false)
}

fn estimate_flat_slice_bytes_inner(
    f: &FlatColumn,
    offset: usize,
    len: usize,
    materialized: bool,
) -> usize {
    let col_len = f.len();
    let offset = offset.min(col_len);
    let len = len.min(col_len - offset);
    if len == 0 {
        return 0;
    }
    let null_bytes = if f.has_nulls() { len.div_ceil(8) } else { 0 };
    let end = offset.saturating_add(len);
    let data_bytes = match f.data() {
        FlatData::Null(_) => 0,
        FlatData::Bool(_) => len.div_ceil(8),
        FlatData::I64(_)
        | FlatData::TimestampNs { .. }
        | FlatData::F64(_)
        | FlatData::FixedBytes8(_) => len.saturating_mul(8),
        FlatData::Date(_) => len.saturating_mul(4),
        FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
            len.saturating_mul(16)
        }
        FlatData::Utf8(u) => {
            let offs = u.offsets();
            let s = offs.get(offset).copied().unwrap_or(0) as usize;
            let e = offs.get(end).copied().unwrap_or(0) as usize;
            e.saturating_sub(s).saturating_add(len.saturating_mul(4))
        }
        FlatData::Binary(b) => {
            let offs = b.offsets();
            let s = offs.get(offset).copied().unwrap_or(0) as usize;
            let e = offs.get(end).copied().unwrap_or(0) as usize;
            e.saturating_sub(s).saturating_add(len.saturating_mul(4))
        }
        FlatData::List {
            offsets, values, ..
        } => {
            let s = offsets.get(offset).copied().unwrap_or(0) as usize;
            let e = offsets.get(end).copied().unwrap_or(0) as usize;
            let child_len = e.saturating_sub(s);
            let child_bytes = if materialized {
                estimate_column_slice_materialized_bytes(values, s, child_len)
            } else {
                estimate_column_slice_bytes(values, s, child_len)
            };
            len.saturating_mul(4).saturating_add(child_bytes)
        }
        FlatData::MapUtf8(m) => {
            let offs = m.offsets();
            let es = offs.get(offset).copied().unwrap_or(0) as usize;
            let ee = offs.get(end).copied().unwrap_or(0) as usize;
            let k_offs = m.keys().offsets();
            let v_offs = m.values().offsets();
            let ks = k_offs.get(es).copied().unwrap_or(0) as usize;
            let ke = k_offs.get(ee).copied().unwrap_or(0) as usize;
            let vs = v_offs.get(es).copied().unwrap_or(0) as usize;
            let ve = v_offs.get(ee).copied().unwrap_or(0) as usize;
            len.saturating_mul(4)
                .saturating_add(ke.saturating_sub(ks))
                .saturating_add(ve.saturating_sub(vs))
                .saturating_add(ee.saturating_sub(es).saturating_mul(8))
        }
        FlatData::Events(e) => {
            let offs = e.offsets();
            let es = offs.get(offset).copied().unwrap_or(0) as usize;
            let ee = offs.get(end).copied().unwrap_or(0) as usize;
            let n_offs = e.names().offsets();
            let a_offs = e.attributes().offsets();
            let ns = n_offs.get(es).copied().unwrap_or(0) as usize;
            let ne = n_offs.get(ee).copied().unwrap_or(0) as usize;
            let as_ = a_offs.get(es).copied().unwrap_or(0) as usize;
            let ae = a_offs.get(ee).copied().unwrap_or(0) as usize;
            len.saturating_mul(4)
                .saturating_add(ee.saturating_sub(es).saturating_mul(8))
                .saturating_add(ne.saturating_sub(ns))
                .saturating_add(ae.saturating_sub(as_))
        }
        FlatData::Links(l) => {
            let offs = l.offsets();
            let es = offs.get(offset).copied().unwrap_or(0) as usize;
            let ee = offs.get(end).copied().unwrap_or(0) as usize;
            let a_offs = l.attributes().offsets();
            let as_ = a_offs.get(es).copied().unwrap_or(0) as usize;
            let ae = a_offs.get(ee).copied().unwrap_or(0) as usize;
            len.saturating_mul(4)
                .saturating_add(ee.saturating_sub(es).saturating_mul(24))
                .saturating_add(ae.saturating_sub(as_))
        }
    };
    data_bytes.saturating_add(null_bytes)
}

pub(crate) fn estimate_column_bytes(col: &Column) -> usize {
    match col {
        Column::Const { value, len } => scalar_byte_len(value).saturating_mul(*len),
        Column::Dict(d) => {
            let codes = d.codes();
            let vals = d.values();
            let mat_bytes = match vals.data() {
                FlatData::Null(_) => 0,
                FlatData::Bool(_) => codes.len().div_ceil(8),
                FlatData::I64(_)
                | FlatData::TimestampNs { .. }
                | FlatData::F64(_)
                | FlatData::FixedBytes8(_) => codes.len().saturating_mul(8),
                FlatData::Date(_) => codes.len().saturating_mul(4),
                FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
                    codes.len().saturating_mul(16)
                }
                FlatData::Utf8(u) => {
                    let offs = u.offsets();
                    let bytes: usize = codes
                        .iter()
                        .map(|&c| {
                            let i = c as usize;
                            offs.get(i + 1)
                                .copied()
                                .unwrap_or(0)
                                .saturating_sub(offs.get(i).copied().unwrap_or(0))
                                as usize
                        })
                        .fold(0usize, usize::saturating_add);
                    bytes.saturating_add(codes.len().saturating_mul(4))
                }
                FlatData::Binary(b) => {
                    let offs = b.offsets();
                    let bytes: usize = codes
                        .iter()
                        .map(|&c| {
                            let i = c as usize;
                            offs.get(i + 1)
                                .copied()
                                .unwrap_or(0)
                                .saturating_sub(offs.get(i).copied().unwrap_or(0))
                                as usize
                        })
                        .fold(0usize, usize::saturating_add);
                    bytes.saturating_add(codes.len().saturating_mul(4))
                }
                _ => codes
                    .iter()
                    .map(|&c| estimate_flat_slice_bytes(vals, c as usize, 1))
                    .fold(0usize, usize::saturating_add),
            };
            let null_bytes = if d.nulls().is_some() || vals.has_nulls() {
                codes.len().div_ceil(8)
            } else {
                0
            };
            codes
                .len()
                .saturating_mul(4)
                .saturating_add(mat_bytes)
                .saturating_add(null_bytes)
        }
        Column::Rle(r) => {
            let vals = r.values();
            let run_ends = r.run_ends();
            let total_rows = r.len();
            let mat_bytes = match vals.data() {
                FlatData::Null(_) => 0,
                FlatData::Bool(_) => total_rows.div_ceil(8),
                FlatData::I64(_)
                | FlatData::TimestampNs { .. }
                | FlatData::F64(_)
                | FlatData::FixedBytes8(_) => total_rows.saturating_mul(8),
                FlatData::Date(_) => total_rows.saturating_mul(4),
                FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
                    total_rows.saturating_mul(16)
                }
                _ => {
                    let mut prev = 0usize;
                    let mut sum = 0usize;
                    for (run_idx, &end) in run_ends.iter().enumerate() {
                        let end_usize = end as usize;
                        let run_len = end_usize.saturating_sub(prev);
                        prev = end_usize;
                        sum = sum.saturating_add(
                            estimate_flat_slice_bytes(vals, run_idx, 1).saturating_mul(run_len),
                        );
                    }
                    sum
                }
            };
            let null_bytes = if vals.has_nulls() {
                total_rows.div_ceil(8)
            } else {
                0
            };
            run_ends
                .len()
                .saturating_mul(12)
                .saturating_add(mat_bytes)
                .saturating_add(null_bytes)
        }
        Column::Flat(f) => estimate_flat_column_bytes(f),
    }
}

pub(super) fn estimate_column_slice_bytes(col: &Column, offset: usize, len: usize) -> usize {
    let col_len = col.len();
    let offset = offset.min(col_len);
    let len = len.min(col_len - offset);
    if len == 0 {
        return 0;
    }
    match col {
        Column::Const { .. } => 64,
        Column::Dict(_) => len.saturating_mul(4),
        Column::Rle(r) => {
            let total = col_len.max(1);
            r.run_ends()
                .len()
                .saturating_mul(12)
                .saturating_mul(len)
                .div_ceil(total)
        }
        Column::Flat(f) => estimate_flat_slice_bytes(f, offset, len),
    }
}

fn count_intersecting_runs(run_ends: &[u32], offset: usize, slice_end: usize) -> usize {
    if offset >= slice_end || run_ends.is_empty() {
        return 0;
    }
    let first = run_ends.partition_point(|&e| (e as usize) <= offset);
    if first >= run_ends.len() {
        return 0;
    }
    let last = run_ends
        .partition_point(|&e| (e as usize) < slice_end)
        .min(run_ends.len() - 1);
    last.saturating_sub(first).saturating_add(1)
}

/// Conservative materialized byte estimate for a slice `offset..offset + len` of `col`,
/// mirroring [`estimate_column_bytes`] for `Const`/`Dict`/`Rle`/`Flat` (including `List<Dict>`).
pub(super) fn estimate_column_slice_materialized_bytes(
    col: &Column,
    offset: usize,
    len: usize,
) -> usize {
    let col_len = col.len();
    let offset = offset.min(col_len);
    let len = len.min(col_len - offset);
    if len == 0 {
        return 0;
    }
    match col {
        Column::Flat(f) => estimate_flat_slice_bytes_inner(f, offset, len, true),
        Column::Const { value, .. } => scalar_byte_len(value).saturating_mul(len),
        Column::Dict(d) => {
            let codes = d.codes();
            let slice_end = offset.saturating_add(len).min(codes.len());
            let slice_start = offset.min(slice_end);
            let sub_codes = &codes[slice_start..slice_end];
            let sub_len = sub_codes.len();
            let vals = d.values();
            let mat_bytes = match vals.data() {
                FlatData::Null(_) => 0,
                FlatData::Bool(_) => sub_len.div_ceil(8),
                FlatData::I64(_)
                | FlatData::TimestampNs { .. }
                | FlatData::F64(_)
                | FlatData::FixedBytes8(_) => sub_len.saturating_mul(8),
                FlatData::Date(_) => sub_len.saturating_mul(4),
                FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
                    sub_len.saturating_mul(16)
                }
                FlatData::Utf8(u) => {
                    let offs = u.offsets();
                    let bytes: usize = sub_codes
                        .iter()
                        .map(|&c| {
                            let i = c as usize;
                            offs.get(i + 1)
                                .copied()
                                .unwrap_or(0)
                                .saturating_sub(offs.get(i).copied().unwrap_or(0))
                                as usize
                        })
                        .fold(0usize, usize::saturating_add);
                    bytes.saturating_add(sub_len.saturating_mul(4))
                }
                FlatData::Binary(b) => {
                    let offs = b.offsets();
                    let bytes: usize = sub_codes
                        .iter()
                        .map(|&c| {
                            let i = c as usize;
                            offs.get(i + 1)
                                .copied()
                                .unwrap_or(0)
                                .saturating_sub(offs.get(i).copied().unwrap_or(0))
                                as usize
                        })
                        .fold(0usize, usize::saturating_add);
                    bytes.saturating_add(sub_len.saturating_mul(4))
                }
                _ => sub_codes
                    .iter()
                    .map(|&c| estimate_flat_slice_bytes_inner(vals, c as usize, 1, true))
                    .fold(0usize, usize::saturating_add),
            };
            let null_bytes = if d.nulls().is_some() || vals.has_nulls() {
                sub_len.div_ceil(8)
            } else {
                0
            };
            sub_len
                .saturating_mul(4)
                .saturating_add(mat_bytes)
                .saturating_add(null_bytes)
        }
        Column::Rle(r) => {
            let vals = r.values();
            let run_ends = r.run_ends();
            let slice_end = offset.saturating_add(len);
            let (intersecting_runs, mat_bytes) = match vals.data() {
                FlatData::Null(_) => (count_intersecting_runs(run_ends, offset, slice_end), 0),
                FlatData::Bool(_) => (
                    count_intersecting_runs(run_ends, offset, slice_end),
                    len.div_ceil(8),
                ),
                FlatData::I64(_)
                | FlatData::TimestampNs { .. }
                | FlatData::F64(_)
                | FlatData::FixedBytes8(_) => (
                    count_intersecting_runs(run_ends, offset, slice_end),
                    len.saturating_mul(8),
                ),
                FlatData::Date(_) => (
                    count_intersecting_runs(run_ends, offset, slice_end),
                    len.saturating_mul(4),
                ),
                FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
                    (
                        count_intersecting_runs(run_ends, offset, slice_end),
                        len.saturating_mul(16),
                    )
                }
                _ => {
                    let first_idx = run_ends.partition_point(|&e| (e as usize) <= offset);
                    let mut prev = if first_idx == 0 {
                        0usize
                    } else {
                        run_ends[first_idx - 1] as usize
                    };
                    let mut runs = 0usize;
                    let mut sum = 0usize;
                    for (rel_idx, &end) in run_ends[first_idx..].iter().enumerate() {
                        let run_idx = first_idx + rel_idx;
                        let end_usize = end as usize;
                        let lo = prev.max(offset);
                        let hi = end_usize.min(slice_end);
                        if hi > lo {
                            runs = runs.saturating_add(1);
                            sum = sum.saturating_add(
                                estimate_flat_slice_bytes_inner(vals, run_idx, 1, true)
                                    .saturating_mul(hi - lo),
                            );
                        }
                        prev = end_usize;
                        if prev >= slice_end {
                            break;
                        }
                    }
                    (runs, sum)
                }
            };
            let null_bytes = if vals.has_nulls() { len.div_ceil(8) } else { 0 };
            intersecting_runs
                .saturating_mul(12)
                .saturating_add(mat_bytes)
                .saturating_add(null_bytes)
        }
    }
}

const SAMPLE_FIXED_ROWS: usize = 192;
const SAMPLE_OFFSET_COUNT: usize = 1024;
const STORED_PROBE_MAX_DEPTH: usize = 16;

/// Reusable scratch buffers for zero-allocation LZ4 stored-size probing.
#[derive(Debug, Default)]
pub(super) struct StoredProbeScratch {
    sample: Vec<u8>,
    lz4_out: Vec<u8>,
}

fn compress_sample_len(scratch: &mut StoredProbeScratch) -> usize {
    if scratch.sample.is_empty() {
        return 0;
    }
    let max_len = lz4_flex::block::get_maximum_output_size(scratch.sample.len());
    if scratch.lz4_out.len() < max_len {
        scratch.lz4_out.resize(max_len, 0);
    }
    lz4_flex::block::compress_into(&scratch.sample, &mut scratch.lz4_out)
        .unwrap_or(scratch.sample.len())
}

fn three_windows(start: usize, len: usize, budget: usize) -> [(usize, usize); 3] {
    if len == 0 || budget == 0 {
        return [(0, 0); 3];
    }
    if len <= budget {
        return [(start, len), (0, 0), (0, 0)];
    }
    let w0 = budget.saturating_add(2) / 3;
    let w1 = budget.saturating_add(1) / 3;
    let w2 = budget / 3;
    let s0 = start;
    let s2 = if w2 > 0 {
        start.saturating_add(len - w2)
    } else {
        0
    };
    let mid_space = len.saturating_sub(w0).saturating_sub(w2);
    let s1 = if w1 > 0 {
        start
            .saturating_add(w0)
            .saturating_add(mid_space.saturating_sub(w1) / 2)
    } else {
        0
    };
    [(s0, w0), (s1, w1), (s2, w2)]
}

fn sample_delta_offsets(
    offsets: &[u32],
    start_idx: usize,
    count: usize,
    scratch: &mut StoredProbeScratch,
) -> usize {
    let max_pairs = offsets.len().saturating_sub(1);
    let start = start_idx.min(max_pairs);
    let count = count.min(max_pairs - start);
    if count == 0 {
        return 0;
    }
    let raw = count.saturating_mul(4);
    let windows = three_windows(start, count, SAMPLE_OFFSET_COUNT);
    scratch.sample.clear();
    let mut sampled = 0usize;
    for (ws, wl) in windows {
        for i in ws..ws.saturating_add(wl) {
            let delta = offsets[i + 1].saturating_sub(offsets[i]);
            scratch.sample.extend_from_slice(&delta.to_le_bytes());
            sampled = sampled.saturating_add(1);
        }
    }
    if sampled == 0 {
        return 0;
    }
    let nominal = sampled.saturating_mul(4);
    let compressed = compress_sample_len(scratch);
    let est = ((raw as u128).saturating_mul(compressed.min(nominal) as u128)
        / (nominal.max(1) as u128)) as usize;
    est.clamp(1.min(raw), raw)
}

fn sample_fixed_slice<T>(
    vals: &[T],
    nulls: Option<&cetra_column::Bitmap>,
    offset: usize,
    len: usize,
    elem_width: usize,
    write_elem: impl Fn(&T, &mut Vec<u8>),
    scratch: &mut StoredProbeScratch,
) -> usize {
    let start = offset.min(vals.len());
    let len = len.min(vals.len() - start);
    if len == 0 {
        return 0;
    }
    let windows = three_windows(start, len, SAMPLE_FIXED_ROWS);
    let sampled_rows: usize = windows.iter().map(|&(_, wl)| wl).sum();
    if sampled_rows == 0 {
        return 0;
    }
    scratch.sample.clear();
    let validity_bytes = if nulls.is_some() {
        sampled_rows.div_ceil(8)
    } else {
        0
    };
    if let Some(bm) = nulls {
        scratch.sample.resize(validity_bytes, 0);
        let mut bit_idx = 0usize;
        let mut non_null_rows = 0usize;
        for (ws, wl) in windows {
            for r in ws..ws.saturating_add(wl) {
                if bm.get(r) {
                    scratch.sample[bit_idx / 8] |= 1u8 << (bit_idx % 8);
                    non_null_rows = non_null_rows.saturating_add(1);
                }
                bit_idx = bit_idx.saturating_add(1);
            }
        }
        if len <= SAMPLE_FIXED_ROWS && non_null_rows == 0 {
            return 0;
        }
        for (ws, wl) in windows {
            let we = ws.saturating_add(wl);
            for (rel, val) in vals[ws..we].iter().enumerate() {
                if bm.get(ws + rel) {
                    write_elem(val, &mut scratch.sample);
                }
            }
        }
    } else {
        for (ws, wl) in windows {
            let we = ws.saturating_add(wl);
            for val in &vals[ws..we] {
                write_elem(val, &mut scratch.sample);
            }
        }
    }
    let raw = len
        .saturating_mul(elem_width)
        .saturating_add(if nulls.is_some() { len.div_ceil(8) } else { 0 });
    let nominal = sampled_rows
        .saturating_mul(elem_width)
        .saturating_add(validity_bytes);
    let compressed = compress_sample_len(scratch);
    let est = ((raw as u128).saturating_mul(compressed.min(nominal) as u128)
        / (nominal.max(1) as u128)) as usize;
    est.clamp(1.min(raw), raw)
}

fn sample_bool_slice(
    b: &cetra_column::Bitmap,
    nulls: Option<&cetra_column::Bitmap>,
    offset: usize,
    len: usize,
    scratch: &mut StoredProbeScratch,
) -> usize {
    let start = offset.min(b.len());
    let len = len.min(b.len() - start);
    if len == 0 {
        return 0;
    }
    let windows = three_windows(start, len, SAMPLE_FIXED_ROWS);
    let sampled_rows: usize = windows.iter().map(|&(_, wl)| wl).sum();
    if sampled_rows == 0 {
        return 0;
    }
    let raw = len
        .div_ceil(8)
        .saturating_add(if nulls.is_some() { len.div_ceil(8) } else { 0 });
    let validity_bytes = if nulls.is_some() {
        sampled_rows.div_ceil(8)
    } else {
        0
    };
    let nominal = sampled_rows.div_ceil(8).saturating_add(validity_bytes);
    scratch.sample.clear();
    if let Some(bm) = nulls {
        scratch.sample.resize(validity_bytes, 0);
        let mut bit_idx = 0usize;
        let mut non_null_rows = 0usize;
        for (ws, wl) in windows {
            for r in ws..ws.saturating_add(wl) {
                if bm.get(r) {
                    scratch.sample[bit_idx / 8] |= 1u8 << (bit_idx % 8);
                    non_null_rows = non_null_rows.saturating_add(1);
                }
                bit_idx = bit_idx.saturating_add(1);
            }
        }
        if len <= SAMPLE_FIXED_ROWS && non_null_rows == 0 {
            return 0;
        }
        let val_bytes = non_null_rows.div_ceil(8);
        scratch
            .sample
            .resize(validity_bytes.saturating_add(val_bytes), 0);
        let mut v_idx = 0usize;
        for (ws, wl) in windows {
            for r in ws..ws.saturating_add(wl) {
                if bm.get(r) {
                    if b.get(r) {
                        scratch.sample[validity_bytes + v_idx / 8] |= 1u8 << (v_idx % 8);
                    }
                    v_idx = v_idx.saturating_add(1);
                }
            }
        }
    } else {
        let val_bytes = sampled_rows.div_ceil(8);
        scratch.sample.resize(val_bytes, 0);
        let mut v_idx = 0usize;
        for (ws, wl) in windows {
            for r in ws..ws.saturating_add(wl) {
                if b.get(r) {
                    scratch.sample[v_idx / 8] |= 1u8 << (v_idx % 8);
                }
                v_idx = v_idx.saturating_add(1);
            }
        }
    }
    let compressed = compress_sample_len(scratch);
    let est = ((raw as u128).saturating_mul(compressed.min(nominal) as u128)
        / (nominal.max(1) as u128)) as usize;
    est.clamp(1.min(raw), raw)
}

fn sample_byte_buffer(
    bytes: &[u8],
    byte_lo: usize,
    byte_hi: usize,
    budget: usize,
    scratch: &mut StoredProbeScratch,
) -> usize {
    let lo = byte_lo.min(bytes.len());
    let hi = byte_hi.min(bytes.len()).max(lo);
    let span = hi - lo;
    if span == 0 {
        return 0;
    }
    let windows = three_windows(lo, span, budget);
    scratch.sample.clear();
    for (ws, wl) in windows {
        if wl > 0 {
            scratch.sample.extend_from_slice(&bytes[ws..ws + wl]);
        }
    }
    let actual_in = scratch.sample.len().max(1);
    let compressed = compress_sample_len(scratch);
    let est = ((span as u128).saturating_mul(compressed.min(actual_in) as u128)
        / (actual_in as u128)) as usize;
    est.clamp(1.min(span), span)
}

fn is_fully_inspected_all_null(
    nulls: Option<&cetra_column::Bitmap>,
    offset: usize,
    len: usize,
) -> bool {
    if let Some(bm) = nulls
        && len <= SAMPLE_FIXED_ROWS
    {
        return (offset..offset.saturating_add(len)).all(|r| !bm.get(r));
    }
    false
}

/// Fast, skew-proof, zero-allocation stored-size estimator for a column slice.
pub(super) fn estimate_column_slice_stored_bytes(
    col: &Column,
    offset: usize,
    len: usize,
    scratch: &mut StoredProbeScratch,
) -> usize {
    estimate_column_slice_stored_bytes_inner(col, offset, len, scratch, 0)
}

fn estimate_column_slice_stored_bytes_inner(
    col: &Column,
    offset: usize,
    len: usize,
    scratch: &mut StoredProbeScratch,
    depth: usize,
) -> usize {
    let col_len = col.len();
    let offset = offset.min(col_len);
    let len = len.min(col_len - offset);
    let raw = estimate_column_slice_bytes(col, offset, len);
    if len == 0 || raw < 64 {
        return raw;
    }
    match col {
        Column::Const { .. } | Column::Rle(_) => raw,
        Column::Dict(d) => sample_fixed_slice(
            d.codes(),
            d.nulls(),
            offset,
            len,
            4,
            |&c, out| out.extend_from_slice(&c.to_le_bytes()),
            scratch,
        )
        .min(raw),
        Column::Flat(f) => {
            let end = offset.saturating_add(len);
            let null_bytes = if f.has_nulls() { len.div_ceil(8) } else { 0 };
            match f.data() {
                FlatData::Null(_) => 0,
                FlatData::Bool(b) => sample_bool_slice(b, f.nulls(), offset, len, scratch).min(raw),
                FlatData::Date(v) => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    4,
                    |&x, out| out.extend_from_slice(&x.to_le_bytes()),
                    scratch,
                )
                .min(raw),
                FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    8,
                    |&x, out| out.extend_from_slice(&x.to_le_bytes()),
                    scratch,
                )
                .min(raw),
                FlatData::F64(v) => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    8,
                    |&x, out| out.extend_from_slice(&x.to_bits().to_le_bytes()),
                    scratch,
                )
                .min(raw),
                FlatData::FixedBytes8(v) => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    8,
                    |x, out| out.extend_from_slice(x),
                    scratch,
                )
                .min(raw),
                FlatData::Decimal128 { values: v, .. } => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    16,
                    |&x, out| out.extend_from_slice(&x.to_le_bytes()),
                    scratch,
                )
                .min(raw),
                FlatData::Interval(v) => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    16,
                    |iv, out| {
                        out.extend_from_slice(&iv.months.to_le_bytes());
                        out.extend_from_slice(&iv.days.to_le_bytes());
                        out.extend_from_slice(&iv.nanos.to_le_bytes());
                    },
                    scratch,
                )
                .min(raw),
                FlatData::FixedBytes16(v) => sample_fixed_slice(
                    v,
                    f.nulls(),
                    offset,
                    len,
                    16,
                    |x, out| out.extend_from_slice(x),
                    scratch,
                )
                .min(raw),
                FlatData::Utf8(u) => {
                    let offs = u.offsets();
                    let byte_lo = offs.get(offset).copied().unwrap_or(0) as usize;
                    let byte_hi = offs.get(end).copied().unwrap_or(0) as usize;
                    let span = byte_hi.saturating_sub(byte_lo);
                    if span == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    let data_est = sample_byte_buffer(u.bytes(), byte_lo, byte_hi, 4096, scratch);
                    let off_est =
                        sample_delta_offsets(offs, offset, len, scratch).saturating_add(null_bytes);
                    data_est.saturating_add(off_est).clamp(1.min(raw), raw)
                }
                FlatData::Binary(b) => {
                    let offs = b.offsets();
                    let byte_lo = offs.get(offset).copied().unwrap_or(0) as usize;
                    let byte_hi = offs.get(end).copied().unwrap_or(0) as usize;
                    let span = byte_hi.saturating_sub(byte_lo);
                    if span == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    let data_est = sample_byte_buffer(b.bytes(), byte_lo, byte_hi, 4096, scratch);
                    let off_est =
                        sample_delta_offsets(offs, offset, len, scratch).saturating_add(null_bytes);
                    data_est.saturating_add(off_est).clamp(1.min(raw), raw)
                }
                FlatData::MapUtf8(m) => {
                    let offs = m.offsets();
                    let es = offs.get(offset).copied().unwrap_or(0) as usize;
                    let ee = offs.get(end).copied().unwrap_or(0) as usize;
                    let entry_count = ee.saturating_sub(es);
                    if entry_count == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    let k_offs = m.keys().offsets();
                    let v_offs = m.values().offsets();
                    let ks = k_offs.get(es).copied().unwrap_or(0) as usize;
                    let ke = k_offs.get(ee).copied().unwrap_or(0) as usize;
                    let vs = v_offs.get(es).copied().unwrap_or(0) as usize;
                    let ve = v_offs.get(ee).copied().unwrap_or(0) as usize;
                    let k_est = sample_byte_buffer(m.keys().bytes(), ks, ke, 2048, scratch);
                    let v_est = sample_byte_buffer(m.values().bytes(), vs, ve, 2048, scratch);
                    let off_est = sample_delta_offsets(offs, offset, len, scratch)
                        .saturating_add(sample_delta_offsets(k_offs, es, entry_count, scratch))
                        .saturating_add(sample_delta_offsets(v_offs, es, entry_count, scratch))
                        .saturating_add(null_bytes);
                    k_est
                        .saturating_add(v_est)
                        .saturating_add(off_est)
                        .clamp(1.min(raw), raw)
                }
                FlatData::Events(e) => {
                    let offs = e.offsets();
                    let es = offs.get(offset).copied().unwrap_or(0) as usize;
                    let ee = offs.get(end).copied().unwrap_or(0) as usize;
                    let event_count = ee.saturating_sub(es);
                    if event_count == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    let t_est = sample_fixed_slice(
                        e.times(),
                        None,
                        es,
                        event_count,
                        8,
                        |&t, out| out.extend_from_slice(&t.to_le_bytes()),
                        scratch,
                    );
                    let n_offs = e.names().offsets();
                    let ns = n_offs.get(es).copied().unwrap_or(0) as usize;
                    let ne = n_offs.get(ee).copied().unwrap_or(0) as usize;
                    let n_est = sample_byte_buffer(e.names().bytes(), ns, ne, 2048, scratch);
                    let a_offs = e.attributes().offsets();
                    let as_ = a_offs.get(es).copied().unwrap_or(0) as usize;
                    let ae = a_offs.get(ee).copied().unwrap_or(0) as usize;
                    let a_est = sample_byte_buffer(e.attributes().bytes(), as_, ae, 2048, scratch);
                    let off_est =
                        sample_delta_offsets(offs, offset, len, scratch).saturating_add(null_bytes);
                    t_est
                        .saturating_add(n_est)
                        .saturating_add(a_est)
                        .saturating_add(off_est)
                        .clamp(1.min(raw), raw)
                }
                FlatData::Links(l) => {
                    let offs = l.offsets();
                    let es = offs.get(offset).copied().unwrap_or(0) as usize;
                    let ee = offs.get(end).copied().unwrap_or(0) as usize;
                    let link_count = ee.saturating_sub(es);
                    if link_count == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    let tid_est = sample_fixed_slice(
                        l.trace_ids(),
                        None,
                        es,
                        link_count,
                        16,
                        |id, out| out.extend_from_slice(id),
                        scratch,
                    );
                    let sid_est = sample_fixed_slice(
                        l.span_ids(),
                        None,
                        es,
                        link_count,
                        8,
                        |id, out| out.extend_from_slice(id),
                        scratch,
                    );
                    let a_offs = l.attributes().offsets();
                    let as_ = a_offs.get(es).copied().unwrap_or(0) as usize;
                    let ae = a_offs.get(ee).copied().unwrap_or(0) as usize;
                    let a_est = sample_byte_buffer(l.attributes().bytes(), as_, ae, 2048, scratch);
                    let off_est =
                        sample_delta_offsets(offs, offset, len, scratch).saturating_add(null_bytes);
                    tid_est
                        .saturating_add(sid_est)
                        .saturating_add(a_est)
                        .saturating_add(off_est)
                        .clamp(1.min(raw), raw)
                }
                FlatData::List {
                    offsets, values, ..
                } => {
                    if depth >= STORED_PROBE_MAX_DEPTH {
                        return raw;
                    }
                    let s = offsets.get(offset).copied().unwrap_or(0) as usize;
                    let e = offsets.get(end).copied().unwrap_or(0) as usize;
                    let child_len = e.saturating_sub(s);
                    if child_len == 0 && is_fully_inspected_all_null(f.nulls(), offset, len) {
                        return 0;
                    }
                    // Complete child recursion before using `scratch` for list offsets.
                    let val_est = estimate_column_slice_stored_bytes_inner(
                        values,
                        s,
                        child_len,
                        scratch,
                        depth + 1,
                    );
                    let off_est = sample_delta_offsets(offsets, offset, len, scratch)
                        .saturating_add(null_bytes);
                    val_est.saturating_add(off_est).clamp(1.min(raw), raw)
                }
            }
        }
    }
}

pub(crate) fn concat_rg_column(
    dt: &DataType,
    pieces: &mut Vec<Column>,
    total_rows: usize,
) -> Result<Column, StatamError> {
    if pieces.len() == 1
        && let Some(only) = pieces.pop()
    {
        return Ok(only);
    }
    let refs: Vec<&Column> = pieces.iter().collect();
    concat_column_refs(dt, &refs, total_rows)
}

pub(crate) fn concat_column_refs(
    dt: &DataType,
    pieces: &[&Column],
    total_rows: usize,
) -> Result<Column, StatamError> {
    if pieces.len() == 1 {
        return Ok(pieces[0].clone());
    }
    // Preserve Column::Const if every piece is an identical Const.
    if let Some(Column::Const {
        value: first_val, ..
    }) = pieces.first().copied()
        && pieces
            .iter()
            .all(|p| matches!(p, Column::Const { value, .. } if value.semantic_eq(first_val)))
    {
        return Ok(Column::try_new_const(first_val.clone(), total_rows)?);
    }

    let mut flats = Vec::with_capacity(pieces.len());
    for &p in pieces {
        flats.push(as_flat_cow(p)?);
    }
    let flat_refs: Vec<&FlatColumn> = flats.iter().map(Cow::as_ref).collect();
    let merged = concat_flat_columns(dt, &flat_refs, total_rows)?;
    Ok(Column::Flat(merged))
}

/// Concatenates flat columns of identical `DataType` into one `FlatColumn`.
pub(crate) fn concat_flat_columns(
    dt: &DataType,
    parts: &[&FlatColumn],
    total_rows: usize,
) -> Result<FlatColumn, StatamError> {
    if parts.len() == 1 {
        return Ok(parts[0].clone());
    }
    if matches!(dt, DataType::Null) {
        return Ok(FlatColumn::try_new(FlatData::Null(total_rows), None)?);
    }

    let has_nulls = parts.iter().any(|p| p.has_nulls());
    let nulls = if has_nulls {
        let mut bm = cetra_column::Bitmap::new_ones(total_rows);
        let mut pos = 0usize;
        for p in parts {
            if let Some(p_nulls) = p.nulls() {
                for i in 0..p.len() {
                    if !p_nulls.get(i) {
                        let _ = bm.set(pos + i, false);
                    }
                }
            }
            pos += p.len();
        }
        if bm.all_ones() { None } else { Some(bm) }
    } else {
        None
    };

    let data = match dt {
        DataType::Null => FlatData::Null(total_rows),
        DataType::Bool => {
            let mut bm = cetra_column::Bitmap::new_zeroed(total_rows);
            let mut pos = 0usize;
            for p in parts {
                if let FlatData::Bool(src) = p.data() {
                    for i in 0..p.len() {
                        if src.get(i) {
                            let _ = bm.set(pos + i, true);
                        }
                    }
                }
                pos += p.len();
            }
            FlatData::Bool(bm)
        }
        DataType::I64 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::I64(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::I64(out)
        }
        DataType::F64 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::F64(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::F64(out)
        }
        DataType::Decimal128 { precision, scale } => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Decimal128 { values, .. } = p.data() {
                    out.extend_from_slice(values);
                }
            }
            FlatData::Decimal128 {
                values: out,
                precision: *precision,
                scale: *scale,
            }
        }
        DataType::TimestampNs { utc } => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::TimestampNs { values, .. } = p.data() {
                    out.extend_from_slice(values);
                }
            }
            FlatData::TimestampNs {
                values: out,
                utc: *utc,
            }
        }
        DataType::Date => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Date(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::Date(out)
        }
        DataType::Interval => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Interval(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::Interval(out)
        }
        DataType::Utf8 => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_bytes = 0usize;
            for p in parts {
                if let FlatData::Utf8(u) = p.data() {
                    total_bytes = total_bytes.checked_add(u.bytes().len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "utf8 concat byte length overflow",
                        },
                    )?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
                reason: "utf8 concat exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total_bytes);
            for p in parts {
                if let FlatData::Utf8(u) = p.data() {
                    let base =
                        u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                            reason: "utf8 concat base offset exceeds u32::MAX",
                        })?;
                    bytes.extend_from_slice(u.bytes());
                    for &o in &u.offsets()[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "utf8 concat offset overflows u32",
                        })?);
                    }
                }
            }
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?)
        }
        DataType::Binary => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_bytes = 0usize;
            for p in parts {
                if let FlatData::Binary(b) = p.data() {
                    total_bytes = total_bytes.checked_add(b.bytes().len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "binary concat byte length overflow",
                        },
                    )?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
                reason: "binary concat exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total_bytes);
            for p in parts {
                if let FlatData::Binary(b) = p.data() {
                    let base =
                        u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                            reason: "binary concat base offset exceeds u32::MAX",
                        })?;
                    bytes.extend_from_slice(b.bytes());
                    for &o in &b.offsets()[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "binary concat offset overflows u32",
                        })?);
                    }
                }
            }
            FlatData::Binary(BinaryData::try_new(offsets, bytes)?)
        }
        DataType::FixedBytes8 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::FixedBytes8(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::FixedBytes8(out)
        }
        DataType::FixedBytes16 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::FixedBytes16(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::FixedBytes16(out)
        }
        DataType::List(inner) => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_child = 0usize;
            let mut child_flats = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::List {
                    offsets: p_off,
                    values,
                    ..
                } = p.data()
                {
                    let base =
                        u32::try_from(total_child).map_err(|_| StatamError::BoundsExceeded {
                            reason: "list concat child offset exceeds u32::MAX",
                        })?;
                    for &o in &p_off[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "list concat offset overflows u32",
                        })?);
                    }
                    total_child = total_child.checked_add(values.len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "list concat child length overflow",
                        },
                    )?;
                    child_flats.push(as_flat_cow(values)?);
                }
            }
            let child_refs: Vec<&FlatColumn> = child_flats.iter().map(Cow::as_ref).collect();
            let merged_child = concat_flat_columns(inner, &child_refs, total_child)?;
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(Column::Flat(merged_child)),
            }
        }
        DataType::MapUtf8 => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_entries = 0usize;
            let mut key_parts = Vec::with_capacity(parts.len());
            let mut val_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::MapUtf8(m) = p.data() {
                    let base =
                        u32::try_from(total_entries).map_err(|_| StatamError::BoundsExceeded {
                            reason: "map concat entry offset exceeds u32::MAX",
                        })?;
                    for &o in &m.offsets()[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "map concat offset overflows u32",
                        })?);
                    }
                    total_entries = total_entries.checked_add(m.entry_count()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "map concat entry count overflow",
                        },
                    )?;
                    key_parts.push(m.keys());
                    val_parts.push(m.values());
                }
            }
            let keys = concat_utf8_data(&key_parts)?;
            let values = concat_utf8_data(&val_parts)?;
            let val_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::MapUtf8(m) => Some((m.entry_count(), m.value_nulls())),
                    _ => None,
                }),
                total_entries,
            );
            FlatData::MapUtf8(Box::new(cetra_column::MapData::try_new(
                offsets, keys, values, val_nulls,
            )?))
        }
        DataType::Events => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_events = 0usize;
            let mut times = Vec::new();
            let mut name_parts = Vec::with_capacity(parts.len());
            let mut attr_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::Events(e) = p.data() {
                    let base =
                        u32::try_from(total_events).map_err(|_| StatamError::BoundsExceeded {
                            reason: "events concat offset exceeds u32::MAX",
                        })?;
                    for &o in &e.offsets()[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "events concat offset overflows u32",
                        })?);
                    }
                    total_events = total_events.checked_add(e.event_count()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "events concat count overflow",
                        },
                    )?;
                    times.extend_from_slice(e.times());
                    name_parts.push(e.names());
                    attr_parts.push(e.attributes());
                }
            }
            let names = concat_utf8_data(&name_parts)?;
            let attributes = concat_utf8_data(&attr_parts)?;
            let attr_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::Events(e) => Some((e.event_count(), e.attribute_nulls())),
                    _ => None,
                }),
                total_events,
            );
            FlatData::Events(Box::new(cetra_column::EventsData::try_new(
                offsets, times, names, attributes, attr_nulls,
            )?))
        }
        DataType::Links => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_links = 0usize;
            let mut trace_ids = Vec::new();
            let mut span_ids = Vec::new();
            let mut attr_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::Links(l) = p.data() {
                    let base =
                        u32::try_from(total_links).map_err(|_| StatamError::BoundsExceeded {
                            reason: "links concat offset exceeds u32::MAX",
                        })?;
                    for &o in &l.offsets()[1..] {
                        offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                            reason: "links concat offset overflows u32",
                        })?);
                    }
                    total_links = total_links.checked_add(l.link_count()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "links concat count overflow",
                        },
                    )?;
                    trace_ids.extend_from_slice(l.trace_ids());
                    span_ids.extend_from_slice(l.span_ids());
                    attr_parts.push(l.attributes());
                }
            }
            let attributes = concat_utf8_data(&attr_parts)?;
            let attr_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::Links(l) => Some((l.link_count(), l.attribute_nulls())),
                    _ => None,
                }),
                total_links,
            );
            FlatData::Links(Box::new(cetra_column::LinksData::try_new(
                offsets, trace_ids, span_ids, attributes, attr_nulls,
            )?))
        }
    };

    Ok(FlatColumn::try_new(data, nulls)?)
}

fn concat_utf8_data(parts: &[&Utf8Data]) -> Result<Utf8Data, StatamError> {
    let total_rows: usize = parts.iter().map(|p| p.len()).sum();
    let total_bytes = parts.iter().try_fold(0usize, |acc, p| {
        acc.checked_add(p.bytes().len())
            .ok_or(StatamError::BoundsExceeded {
                reason: "utf8 concat byte length overflow",
            })
    })?;
    let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
        reason: "utf8 concat exceeds u32::MAX",
    })?;
    let mut offsets = Vec::with_capacity(total_rows + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    for p in parts {
        let base = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "utf8 concat base offset exceeds u32::MAX",
        })?;
        bytes.extend_from_slice(p.bytes());
        for &o in &p.offsets()[1..] {
            offsets.push(base.checked_add(o).ok_or(StatamError::BoundsExceeded {
                reason: "utf8 concat offset overflows u32",
            })?);
        }
    }
    Ok(Utf8Data::try_new(offsets, bytes)?)
}

fn concat_entry_bitmaps<'a>(
    parts: impl Iterator<Item = (usize, Option<&'a cetra_column::Bitmap>)> + Clone,
    total_len: usize,
) -> Option<cetra_column::Bitmap> {
    let any_nulls = parts.clone().any(|(_, bm)| bm.is_some());
    if !any_nulls {
        return None;
    }
    let mut out = cetra_column::Bitmap::new_ones(total_len);
    let mut pos = 0usize;
    for (len, bm) in parts {
        if let Some(b) = bm {
            for i in 0..len {
                if !b.get(i) {
                    let _ = out.set(pos + i, false);
                }
            }
        }
        pos += len;
    }
    if out.all_ones() { None } else { Some(out) }
}
