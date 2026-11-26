//! Column concatenation and slice size estimation helpers for Row Group buffering.

use std::borrow::Cow;

use cetra_column::{BinaryData, Column, DataType, FlatColumn, FlatData, Utf8Data};

use crate::StatamError;

use super::rg_encode::as_flat_cow;

pub(super) fn estimate_column_bytes(col: &Column) -> usize {
    match col {
        Column::Const { .. } => 64,
        Column::Dict(d) => d.codes().len().saturating_mul(4),
        Column::Rle(r) => r.run_ends().len().saturating_mul(12),
        Column::Flat(f) => {
            let null_bytes = f.nulls().map_or(0, |bm| bm.len().div_ceil(8));
            let data_bytes = match f.data() {
                FlatData::Null(_) => 0,
                FlatData::Bool(b) => b.len().div_ceil(8),
                FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
                    v.len().saturating_mul(8)
                }
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
    }
}

pub(super) fn estimate_column_slice_bytes(col: &Column, offset: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    match col {
        Column::Const { .. } => 64,
        Column::Dict(_) => len.saturating_mul(4),
        Column::Rle(r) => {
            let total = col.len().max(1);
            r.run_ends()
                .len()
                .saturating_mul(12)
                .saturating_mul(len)
                .div_ceil(total)
        }
        Column::Flat(f) => {
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
                    len.saturating_mul(4)
                        .saturating_add(estimate_column_slice_bytes(values, s, child_len))
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
    }
}

pub(super) fn concat_rg_column(
    dt: &DataType,
    pieces: &mut Vec<Column>,
    total_rows: usize,
) -> Result<Column, StatamError> {
    if pieces.len() == 1
        && let Some(only) = pieces.pop()
    {
        return Ok(only);
    }
    // Preserve Column::Const if every piece is an identical Const.
    if let Some(Column::Const {
        value: first_val, ..
    }) = pieces.first()
        && pieces
            .iter()
            .all(|p| matches!(p, Column::Const { value, .. } if value.semantic_eq(first_val)))
    {
        return Ok(Column::try_new_const(first_val.clone(), total_rows)?);
    }

    let mut flats = Vec::with_capacity(pieces.len());
    for p in pieces {
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
