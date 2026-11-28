//! Pre-row JSON byte estimation, [`CappedBuffer`], and export piece slicing.

use std::fmt;
use std::io::{self, Write};
use std::ops::Range;

use cetra_column::{
    Batch, Column, EventItem, EventsData, FlatColumn, FlatData, LinkItem, LinksData, MapData,
    Reservation, ScalarValue, Schema, Selection,
};

use crate::error::QueryError;

/// Opening and closing brackets (`[]`) of a JSON array of rows.
pub const JSON_ARRAY_BYTES: u64 = 2;

/// Comma separating consecutive rows in a JSON array.
pub const JSON_ROW_SEPARATOR_BYTES: u64 = 1;

/// JSON bytes of a `null`.
const NULL_BYTES: u64 = 4;

/// Bound on the JSON text of a scalar bool, integer, finite float, timestamp, or date (quotes included).
const VALUE_BYTES: u64 = 40;

/// Bound on the quoted text of a calendar `Interval` (max 76 chars + 2 quotes = 78 <= 96).
const MONTH_DAY_NANO_INTERVAL_BYTES: u64 = 96;

/// Two quotes surrounding a JSON string.
const QUOTES: u64 = 2;

/// Opening and closing brackets of a JSON array or object.
const BRACKETS: u64 = 2;

/// Quoted key overhead per JSON object member (two quotes, colon, comma).
const MEMBER_BYTES: u64 = 4;

/// Comma following an element of a JSON array.
const ELEMENT_BYTES: u64 = 1;

/// Returns true when `estimate` exceeds `limit` bytes.
pub fn exceeds(estimate: u64, limit: usize) -> bool {
    estimate > to_u64(limit)
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Upper bound on the unquoted decimal text length of `Decimal128(precision, scale)`.
fn decimal_digits_bound(precision: u8, scale: i8) -> u64 {
    let precision = u64::from(precision);
    let magnitude = u64::from(scale.unsigned_abs());
    if scale <= 0 {
        return 1 + precision + magnitude;
    }
    precision.max(magnitude) + 3
}

/// Estimated JSON bytes for each active row in `batch` (`len() == batch.num_rows()`).
///
/// Each row is sized independently so batches can be sliced at any row boundary, and nested
/// `List`/`MapUtf8`/`Events`/`Links` values only inspect elements reachable by active rows.
pub fn batch_row_sizes(batch: &Batch) -> Result<Vec<u64>, QueryError> {
    batch_row_sizes_prefix(batch.schema(), batch, batch.num_rows())
}

pub(crate) fn batch_row_sizes_prefix(
    schema: &Schema,
    batch: &Batch,
    count: usize,
) -> Result<Vec<u64>, QueryError> {
    if count > batch.num_rows() {
        return Err(QueryError::internal("row prefix count out of bounds"));
    }
    if schema.len() != batch.columns().len() {
        return Err(QueryError::internal(
            "schema column count does not match batch",
        ));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut totals = vec![BRACKETS; count];
    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let member = to_u64(field.name.len()).saturating_add(MEMBER_BYTES);
        add_column_sizes_for_selection(
            &mut totals,
            col,
            batch.selection(),
            batch.raw_len(),
            count,
            member,
        )?;
    }
    Ok(totals)
}

/// Estimated JSON bytes for every physical row of `col` (`0..col.len()`).
pub fn column_row_sizes(col: &Column) -> Result<Vec<u64>, QueryError> {
    let mut totals = vec![0u64; col.len()];
    add_column_sizes_for_selection(&mut totals, col, &Selection::All, col.len(), col.len(), 0)?;
    Ok(totals)
}

fn add_column_sizes_for_selection(
    totals: &mut [u64],
    col: &Column,
    selection: &Selection,
    raw_len: usize,
    count: usize,
    member_overhead: u64,
) -> Result<(), QueryError> {
    match col {
        Column::Const { value, .. } => {
            let val_size = scalar_value_size(value)?;
            let per_row = val_size.saturating_add(member_overhead);
            for total in totals.iter_mut() {
                *total = total.saturating_add(per_row);
            }
            Ok(())
        }
        Column::Flat(fc) => {
            add_flat_sizes_selected(totals, fc, selection, raw_len, count, member_overhead)
        }
        Column::Dict(dict) => {
            let mut cache: Vec<Option<u64>> = vec![None; dict.values().len()];
            let mut row_out = 0usize;
            for_each_selected(selection, raw_len, count, |phys| {
                let sz = if dict.nulls().is_some_and(|bm| !bm.get(phys)) {
                    NULL_BYTES
                } else {
                    let code = *dict
                        .codes()
                        .get(phys)
                        .ok_or_else(|| QueryError::internal("dict code out of bounds"))?
                        as usize;
                    let slot = cache
                        .get_mut(code)
                        .ok_or_else(|| QueryError::internal("dict value index out of bounds"))?;
                    match *slot {
                        Some(cached) => cached,
                        None => {
                            let computed = flat_row_size(dict.values(), code)?;
                            *slot = Some(computed);
                            computed
                        }
                    }
                };
                totals[row_out] = totals[row_out]
                    .saturating_add(sz)
                    .saturating_add(member_overhead);
                row_out += 1;
                Ok(())
            })
        }
        Column::Rle(rle) => {
            let mut cache: Vec<Option<u64>> = vec![None; rle.values().len()];
            let mut row_out = 0usize;
            for_each_selected(selection, raw_len, count, |phys| {
                if phys >= rle.len() {
                    return Err(QueryError::internal("rle row index out of bounds"));
                }
                let run_idx = rle
                    .run_ends()
                    .partition_point(|&end| (end as usize) <= phys);
                let slot = cache
                    .get_mut(run_idx)
                    .ok_or_else(|| QueryError::internal("rle run index out of bounds"))?;
                let sz = match *slot {
                    Some(cached) => cached,
                    None => {
                        let computed = flat_row_size(rle.values(), run_idx)?;
                        *slot = Some(computed);
                        computed
                    }
                };
                totals[row_out] = totals[row_out]
                    .saturating_add(sz)
                    .saturating_add(member_overhead);
                row_out += 1;
                Ok(())
            })
        }
    }
}

fn add_flat_sizes_selected(
    totals: &mut [u64],
    fc: &FlatColumn,
    selection: &Selection,
    raw_len: usize,
    count: usize,
    member_overhead: u64,
) -> Result<(), QueryError> {
    if let Some(fixed) = fixed_data_size(fc.data()) {
        let mut row_out = 0usize;
        return for_each_selected(selection, raw_len, count, |phys| {
            let sz = if fc.is_null(phys) { NULL_BYTES } else { fixed };
            totals[row_out] = totals[row_out]
                .saturating_add(sz)
                .saturating_add(member_overhead);
            row_out += 1;
            Ok(())
        });
    }

    let mut row_out = 0usize;
    for_each_selected(selection, raw_len, count, |phys| {
        let sz = flat_row_size(fc, phys)?;
        totals[row_out] = totals[row_out]
            .saturating_add(sz)
            .saturating_add(member_overhead);
        row_out += 1;
        Ok(())
    })
}

fn for_each_selected(
    selection: &Selection,
    raw_len: usize,
    count: usize,
    mut f: impl FnMut(usize) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    match selection {
        Selection::All => {
            if count > raw_len {
                return Err(QueryError::internal("selection count out of bounds"));
            }
            for phys in 0..count {
                f(phys)?;
            }
        }
        Selection::Indices(idx) => {
            if count > idx.len() {
                return Err(QueryError::internal(
                    "selection indices count out of bounds",
                ));
            }
            for &phys in &idx[..count] {
                f(phys as usize)?;
            }
        }
        Selection::Mask(bm) => {
            let mut seen = 0usize;
            for phys in bm.iter_ones().take(count) {
                f(phys)?;
                seen += 1;
            }
            if seen < count {
                return Err(QueryError::internal("selection mask count out of bounds"));
            }
        }
    }
    Ok(())
}

fn fixed_data_size(data: &FlatData) -> Option<u64> {
    match data {
        FlatData::Null(_) => Some(NULL_BYTES),
        FlatData::Bool(_)
        | FlatData::I64(_)
        | FlatData::F64(_)
        | FlatData::TimestampNs { .. }
        | FlatData::Date(_) => Some(VALUE_BYTES),
        FlatData::Interval(_) => Some(MONTH_DAY_NANO_INTERVAL_BYTES),
        FlatData::Decimal128 {
            precision, scale, ..
        } => Some(decimal_digits_bound(*precision, *scale).saturating_add(QUOTES)),
        FlatData::FixedBytes8(_) => Some(8 * 2 + QUOTES),
        FlatData::FixedBytes16(_) => Some(16 * 2 + QUOTES),
        FlatData::Utf8(_)
        | FlatData::Binary(_)
        | FlatData::List { .. }
        | FlatData::MapUtf8(_)
        | FlatData::Events(_)
        | FlatData::Links(_) => None,
    }
}

fn flat_row_size(fc: &FlatColumn, row: usize) -> Result<u64, QueryError> {
    if row >= fc.len() {
        return Err(QueryError::internal("flat column row out of bounds"));
    }
    if fc.is_null(row) {
        return Ok(NULL_BYTES);
    }
    if let Some(fixed) = fixed_data_size(fc.data()) {
        return Ok(fixed);
    }
    match fc.data() {
        FlatData::Utf8(u) => {
            let offsets = u.offsets();
            let lo = *offsets
                .get(row)
                .ok_or_else(|| QueryError::internal("utf8 offset out of bounds"))?;
            let hi = *offsets
                .get(row + 1)
                .ok_or_else(|| QueryError::internal("utf8 offset out of bounds"))?;
            Ok(u64::from(hi.saturating_sub(lo)).saturating_add(QUOTES))
        }
        FlatData::Binary(b) => {
            let offsets = b.offsets();
            let lo = *offsets
                .get(row)
                .ok_or_else(|| QueryError::internal("binary offset out of bounds"))?;
            let hi = *offsets
                .get(row + 1)
                .ok_or_else(|| QueryError::internal("binary offset out of bounds"))?;
            Ok(u64::from(hi.saturating_sub(lo))
                .saturating_mul(2)
                .saturating_add(QUOTES))
        }
        FlatData::List {
            offsets, values, ..
        } => {
            let lo = *offsets
                .get(row)
                .ok_or_else(|| QueryError::internal("list offset out of bounds"))?
                as usize;
            let hi = *offsets
                .get(row + 1)
                .ok_or_else(|| QueryError::internal("list offset out of bounds"))?
                as usize;
            if lo > hi || hi > values.len() {
                return Err(QueryError::internal("invalid list offsets"));
            }
            let mut sum = BRACKETS;
            for idx in lo..hi {
                let elem_sz = column_single_row_size(values, idx)?;
                sum = sum.saturating_add(elem_sz).saturating_add(ELEMENT_BYTES);
            }
            Ok(sum)
        }
        FlatData::MapUtf8(map) => map_row_size(map, row),
        FlatData::Events(events) => events_row_size(events, row),
        FlatData::Links(links) => links_row_size(links, row),
        _ => Err(QueryError::internal("unexpected flat column variant")),
    }
}

fn column_single_row_size(col: &Column, row: usize) -> Result<u64, QueryError> {
    match col {
        Column::Flat(fc) => flat_row_size(fc, row),
        Column::Const { value, len } => {
            if row >= *len {
                return Err(QueryError::internal("const column row out of bounds"));
            }
            scalar_value_size(value)
        }
        Column::Dict(dict) => {
            if row >= dict.len() {
                return Err(QueryError::internal("dict column row out of bounds"));
            }
            if dict.nulls().is_some_and(|bm| !bm.get(row)) {
                return Ok(NULL_BYTES);
            }
            let code = dict.codes()[row] as usize;
            flat_row_size(dict.values(), code)
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return Err(QueryError::internal("rle column row out of bounds"));
            }
            let run_idx = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            flat_row_size(rle.values(), run_idx)
        }
    }
}

fn map_row_size(map: &MapData, row: usize) -> Result<u64, QueryError> {
    let lo = *map
        .offsets()
        .get(row)
        .ok_or_else(|| QueryError::internal("map offset out of bounds"))? as usize;
    let hi = *map
        .offsets()
        .get(row + 1)
        .ok_or_else(|| QueryError::internal("map offset out of bounds"))? as usize;
    if lo > hi || hi > map.keys().len() || map.keys().len() != map.values().len() {
        return Err(QueryError::internal("invalid map offsets"));
    }
    let key_offsets = map.keys().offsets();
    let val_offsets = map.values().offsets();
    let mut sum = BRACKETS;
    for idx in lo..hi {
        let k_len = u64::from(key_offsets[idx + 1].saturating_sub(key_offsets[idx]));
        let v_size = if map.value_nulls().is_some_and(|bm| !bm.get(idx)) {
            NULL_BYTES
        } else {
            u64::from(val_offsets[idx + 1].saturating_sub(val_offsets[idx])).saturating_add(QUOTES)
        };
        sum = sum
            .saturating_add(k_len.saturating_add(QUOTES))
            .saturating_add(v_size)
            .saturating_add(MEMBER_BYTES);
    }
    Ok(sum)
}

fn events_row_size(events: &EventsData, row: usize) -> Result<u64, QueryError> {
    let lo = *events
        .offsets()
        .get(row)
        .ok_or_else(|| QueryError::internal("events offset out of bounds"))? as usize;
    let hi = *events
        .offsets()
        .get(row + 1)
        .ok_or_else(|| QueryError::internal("events offset out of bounds"))? as usize;
    if lo > hi || hi > events.event_count() {
        return Err(QueryError::internal("invalid events offsets"));
    }
    let name_offsets = events.names().offsets();
    let attr_offsets = events.attributes().offsets();
    let mut sum = BRACKETS;
    for idx in lo..hi {
        let name_len = u64::from(name_offsets[idx + 1].saturating_sub(name_offsets[idx]));
        let attr_len = if events.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
            None
        } else {
            Some(u64::from(
                attr_offsets[idx + 1].saturating_sub(attr_offsets[idx]),
            ))
        };
        let item_size = event_item_size(name_len, attr_len);
        sum = sum.saturating_add(item_size).saturating_add(ELEMENT_BYTES);
    }
    Ok(sum)
}

fn links_row_size(links: &LinksData, row: usize) -> Result<u64, QueryError> {
    let lo = *links
        .offsets()
        .get(row)
        .ok_or_else(|| QueryError::internal("links offset out of bounds"))? as usize;
    let hi = *links
        .offsets()
        .get(row + 1)
        .ok_or_else(|| QueryError::internal("links offset out of bounds"))? as usize;
    if lo > hi || hi > links.link_count() {
        return Err(QueryError::internal("invalid links offsets"));
    }
    let attr_offsets = links.attributes().offsets();
    let mut sum = BRACKETS;
    for idx in lo..hi {
        let attr_len = if links.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
            None
        } else {
            Some(u64::from(
                attr_offsets[idx + 1].saturating_sub(attr_offsets[idx]),
            ))
        };
        let item_size = link_item_size(attr_len);
        sum = sum.saturating_add(item_size).saturating_add(ELEMENT_BYTES);
    }
    Ok(sum)
}

fn event_item_size(name_len: u64, attr_len: Option<u64>) -> u64 {
    // {"time":<40>,"name":<QUOTES+name_len>,"attributes":<NULL or QUOTES+attr_len>}
    let time_member = 4 + MEMBER_BYTES + VALUE_BYTES;
    let name_member = 4 + MEMBER_BYTES + QUOTES.saturating_add(name_len);
    let attr_val = attr_len.map_or(NULL_BYTES, |len| QUOTES.saturating_add(len));
    let attr_member = 10 + MEMBER_BYTES + attr_val;
    BRACKETS
        .saturating_add(time_member)
        .saturating_add(name_member)
        .saturating_add(attr_member)
}

fn link_item_size(attr_len: Option<u64>) -> u64 {
    // {"trace_id":<34>,"span_id":<18>,"attributes":<NULL or QUOTES+attr_len>}
    let tid_member = 8 + MEMBER_BYTES + (16 * 2 + QUOTES);
    let sid_member = 7 + MEMBER_BYTES + (8 * 2 + QUOTES);
    let attr_val = attr_len.map_or(NULL_BYTES, |len| QUOTES.saturating_add(len));
    let attr_member = 10 + MEMBER_BYTES + attr_val;
    BRACKETS
        .saturating_add(tid_member)
        .saturating_add(sid_member)
        .saturating_add(attr_member)
}

fn scalar_value_size(sv: &ScalarValue) -> Result<u64, QueryError> {
    Ok(match sv {
        ScalarValue::Null(_) => NULL_BYTES,
        ScalarValue::Bool(_)
        | ScalarValue::I64(_)
        | ScalarValue::F64(_)
        | ScalarValue::TimestampNs { .. }
        | ScalarValue::Date(_) => VALUE_BYTES,
        ScalarValue::Interval(_) => MONTH_DAY_NANO_INTERVAL_BYTES,
        ScalarValue::Decimal128 {
            precision, scale, ..
        } => decimal_digits_bound(*precision, *scale).saturating_add(QUOTES),
        ScalarValue::Utf8(s) => to_u64(s.len()).saturating_add(QUOTES),
        ScalarValue::Binary(b) => to_u64(b.len()).saturating_mul(2).saturating_add(QUOTES),
        ScalarValue::FixedBytes8(_) => 8 * 2 + QUOTES,
        ScalarValue::FixedBytes16(_) => 16 * 2 + QUOTES,
        ScalarValue::List { values, .. } => {
            let mut sum = BRACKETS;
            for v in values {
                sum = sum
                    .saturating_add(scalar_value_size(v)?)
                    .saturating_add(ELEMENT_BYTES);
            }
            sum
        }
        ScalarValue::MapUtf8(entries) => {
            let mut sum = BRACKETS;
            for (k, v) in entries {
                let k_size = to_u64(k.len()).saturating_add(QUOTES);
                let v_size = v
                    .as_ref()
                    .map_or(NULL_BYTES, |s| to_u64(s.len()).saturating_add(QUOTES));
                sum = sum
                    .saturating_add(k_size)
                    .saturating_add(v_size)
                    .saturating_add(MEMBER_BYTES);
            }
            sum
        }
        ScalarValue::Events(items) => {
            let mut sum = BRACKETS;
            for EventItem {
                name, attributes, ..
            } in items
            {
                let item_size = event_item_size(
                    to_u64(name.len()),
                    attributes.as_ref().map(|a| to_u64(a.len())),
                );
                sum = sum.saturating_add(item_size).saturating_add(ELEMENT_BYTES);
            }
            sum
        }
        ScalarValue::Links(items) => {
            let mut sum = BRACKETS;
            for LinkItem { attributes, .. } in items {
                let item_size = link_item_size(attributes.as_ref().map(|a| to_u64(a.len())));
                sum = sum.saturating_add(item_size).saturating_add(ELEMENT_BYTES);
            }
            sum
        }
    })
}

/// Output byte buffer implementing [`std::io::Write`] that enforces both `max_bytes`
/// and an optional [`Reservation`], turning overflow or OOM into [`QueryError::TooLarge`].
///
/// `fmt::Debug` prints only byte counts (never encoded cell payloads).
pub struct CappedBuffer<'a> {
    bytes: Vec<u8>,
    max_bytes: usize,
    reservation: Option<&'a mut Reservation>,
    before: usize,
    refused: bool,
    armed: bool,
}

impl fmt::Debug for CappedBuffer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CappedBuffer")
            .field("len", &self.bytes.len())
            .field("max_bytes", &self.max_bytes)
            .field("refused", &self.refused)
            .finish()
    }
}

impl Drop for CappedBuffer<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.rollback();
        }
    }
}

impl<'a> CappedBuffer<'a> {
    /// Creates an empty [`CappedBuffer`] capped at `max_bytes` and backed by `reservation`.
    pub fn new(max_bytes: usize, reservation: Option<&'a mut Reservation>) -> Self {
        let before = reservation.as_ref().map_or(0, |r| r.size());
        Self {
            bytes: Vec::new(),
            max_bytes,
            reservation,
            before,
            refused: false,
            armed: true,
        }
    }

    /// Bytes currently written.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when no bytes have been written.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Remaining byte headroom before `max_bytes`.
    pub fn remaining(&self) -> usize {
        self.max_bytes.saturating_sub(self.bytes.len())
    }

    /// True if a write was refused due to `max_bytes` or `Reservation` limits.
    pub fn was_refused(&self) -> bool {
        self.refused
    }

    /// Appends `buf`, failing with [`QueryError::TooLarge`] if `max_bytes` or `reservation`
    /// would be exceeded (and rolling back the reservation to its pre-buffer size).
    pub fn push_bytes(&mut self, buf: &[u8]) -> Result<(), QueryError> {
        if buf.is_empty() {
            return Ok(());
        }
        let fits = self
            .bytes
            .len()
            .checked_add(buf.len())
            .is_some_and(|n| n <= self.max_bytes);
        if !fits {
            self.refused = true;
            self.rollback();
            return Err(QueryError::TooLarge);
        }
        if let Some(res) = self.reservation.as_deref_mut()
            && res.try_grow(buf.len()).is_err()
        {
            self.refused = true;
            self.rollback();
            return Err(QueryError::TooLarge);
        }
        self.bytes.extend_from_slice(buf);
        Ok(())
    }

    /// Rolls back any bytes reserved by this buffer since creation.
    pub fn rollback(&mut self) {
        if let Some(res) = self.reservation.as_deref_mut() {
            let cur = res.size();
            if cur > self.before {
                res.shrink(cur - self.before);
            }
        }
    }

    /// Consumes the buffer and returns the written bytes, keeping any reservation intact.
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.armed = false;
        std::mem::take(&mut self.bytes)
    }

    /// Runs `build` against an isolated temporary [`CappedBuffer`] sharing this buffer's
    /// remaining byte budget and [`Reservation`], then passes the temporary bytes to
    /// `use_temp` while keeping the temporary reservation held until the buffer is dropped.
    pub(crate) fn with_temp(
        &mut self,
        build: impl FnOnce(&mut CappedBuffer<'_>) -> Result<(), QueryError>,
        use_temp: impl FnOnce(&[u8], &mut CappedBuffer<'_>) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        let outer_bytes = std::mem::take(&mut self.bytes);
        let outer_max = self.max_bytes;
        self.max_bytes = outer_max.saturating_sub(outer_bytes.len());
        let build_res = build(self);
        let temp = std::mem::replace(&mut self.bytes, outer_bytes);
        self.max_bytes = outer_max;
        if let Err(err) = build_res {
            drop(temp);
            self.rollback();
            return Err(err);
        }
        let temp_len = temp.len();
        let use_res = use_temp(&temp, self);
        drop(temp);
        match use_res {
            Ok(()) => {
                if let Some(r) = self.reservation.as_deref_mut() {
                    r.shrink(temp_len);
                }
                Ok(())
            }
            Err(err) => {
                self.rollback();
                Err(err)
            }
        }
    }
}

impl Write for CappedBuffer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.push_bytes(buf) {
            Ok(()) => Ok(buf.len()),
            Err(_) => Err(io::Error::other("encoded output is over its budget")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Slices `batch` into active-row ranges whose estimated JSON bytes stay at most
/// `max_piece_bytes` (while always taking at least 1 row per piece).
///
/// Returns [`QueryError::TooLarge`] if any single row's estimated size exceeds `max_chunk_bytes`.
pub fn export_piece_ranges(
    batch: &Batch,
    max_piece_bytes: usize,
    max_chunk_bytes: usize,
) -> Result<Vec<Range<usize>>, QueryError> {
    let num_rows = batch.num_rows();
    if num_rows == 0 {
        return Ok(Vec::new());
    }
    let piece_cap = max_piece_bytes.min(max_chunk_bytes);
    let sizes = batch_row_sizes(batch)?;
    let mut ranges = Vec::new();
    let mut next = 0usize;
    while next < num_rows {
        let mut total = 0u64;
        let mut keep = 0usize;
        for &sz in &sizes[next..] {
            let next_total = total.saturating_add(sz);
            if keep > 0 && exceeds(next_total, piece_cap) {
                break;
            }
            total = next_total;
            keep += 1;
        }
        if exceeds(total, max_chunk_bytes) {
            return Err(QueryError::TooLarge);
        }
        let end = next + keep;
        ranges.push(next..end);
        next = end;
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        Bitmap, Column, DataType, DictColumn, Field, FlatColumn, FlatData, GlobalPool, Interval,
        RleColumn, Schema, Selection, Utf8Data,
    };

    use super::*;
    use crate::encode::{EXPORT_MAX_CHUNK_BYTES, EXPORT_PIECE_BYTES, encode_ndjson_chunk};

    fn assert_col_estimate_bounds_ndjson(col: Column) {
        let dt = col.data_type();
        let len = col.len();
        let schema = Schema::try_new_arc(vec![Field::new("v", dt.clone(), true)]).unwrap();
        let batch = Batch::try_new_all(schema, vec![col], len).unwrap();
        let est = batch_row_sizes(&batch).unwrap();
        let ndjson = encode_ndjson_chunk(&batch, 0..batch.num_rows(), usize::MAX, None).unwrap();
        let text = String::from_utf8(ndjson).unwrap();
        for (row, line) in text.lines().enumerate() {
            assert!(
                est[row] >= line.len() as u64,
                "{dt:?} row {row}: estimate {} < actual {} ({line})",
                est[row],
                line.len()
            );
        }
    }

    #[test]
    fn decimals_and_intervals_estimate_bounds_actual_json() {
        let nine = |digits: u32| 10_i128.pow(digits) - 1;
        assert_col_estimate_bounds_ndjson(Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![-nine(38), nine(38), 0, -5],
                    precision: 38,
                    scale: 37,
                },
                None,
            )
            .unwrap(),
        ));
        assert_col_estimate_bounds_ndjson(Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![-nine(38), nine(38), 0],
                    precision: 38,
                    scale: -20,
                },
                None,
            )
            .unwrap(),
        ));
        assert_col_estimate_bounds_ndjson(Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![-5, 5, 99, -99, 0],
                    precision: 2,
                    scale: 2,
                },
                None,
            )
            .unwrap(),
        ));
        assert_col_estimate_bounds_ndjson(Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![-5, 5, 0],
                    precision: 1,
                    scale: 1,
                },
                None,
            )
            .unwrap(),
        ));
        assert_col_estimate_bounds_ndjson(Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    Interval {
                        months: i32::MIN,
                        days: i32::MIN,
                        nanos: i64::MIN,
                    },
                    Interval {
                        months: i32::MAX,
                        days: i32::MAX,
                        nanos: i64::MAX,
                    },
                    Interval::default(),
                ]),
                None,
            )
            .unwrap(),
        ));
    }

    #[test]
    fn sliced_list_and_selection_mask_only_size_reachable_rows() {
        let big = "z".repeat(1 << 20);
        let child = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([big.as_str(), "a", "b"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1, 2, 3],
                    values: Box::new(child),
                },
                None,
            )
            .unwrap(),
        );
        let schema = Schema::try_new_arc(vec![Field::new(
            "items",
            DataType::List(Box::new(DataType::Utf8)),
            true,
        )])
        .unwrap();
        let batch = Batch::try_new(
            schema,
            vec![list.clone()],
            3,
            Selection::Mask(Bitmap::from_bools(&[false, true, true])),
        )
        .unwrap();
        let sizes = batch_row_sizes(&batch).unwrap();
        assert_eq!(sizes.len(), 2);
        // Each kept row is {"items":["a"]} -> BRACKETS(2) + member("items", 5+4=9) + list(2 + (1+2) + 1 = 6) = 17.
        assert_eq!(sizes, vec![17, 17]);

        let sliced = list.slice(1, 2, None).unwrap();
        assert_eq!(column_row_sizes(&sliced).unwrap(), vec![6, 6]);
    }

    #[test]
    fn dict_null_slot_and_rle_sizes() {
        let dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["hello", "world"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 99, 1],
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        assert_eq!(column_row_sizes(&dict).unwrap(), vec![7, NULL_BYTES, 7]);

        let rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::I64(vec![10, 0]),
                    Some(Bitmap::from_bools(&[true, false])),
                )
                .unwrap(),
                vec![2, 4],
            )
            .unwrap(),
        );
        assert_eq!(
            column_row_sizes(&rle).unwrap(),
            vec![VALUE_BYTES, VALUE_BYTES, NULL_BYTES, NULL_BYTES]
        );
    }

    #[test]
    fn capped_buffer_enforces_max_bytes_and_reservation_and_redacts_debug() {
        let scope = GlobalPool::standalone(128);
        let mut res = scope.register("buf", false);

        let mut buf = CappedBuffer::new(200, Some(&mut res));
        buf.write_all(b"secret-payload-value").unwrap();
        let dbg = format!("{buf:?}");
        assert!(!dbg.contains("secret-payload-value"), "{dbg}");
        assert!(dbg.contains("len: 20"), "{dbg}");
        assert_eq!(scope.reserved(), 20);

        // Pool cap (128) is hit before max_bytes (200), rolling back reservation.
        let err = buf.push_bytes(&[b'x'; 120]).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        drop(buf);
        assert_eq!(res.size(), 0);

        // max_bytes cap is hit before pool cap.
        let mut res2 = scope.register("buf2", false);
        let mut buf2 = CappedBuffer::new(10, Some(&mut res2));
        let err2 = buf2.write_all(b"0123456789a").unwrap_err();
        assert_eq!(err2.kind(), io::ErrorKind::Other);
        assert!(buf2.was_refused());
        drop(buf2);
        assert_eq!(res2.size(), 0);
    }

    #[test]
    fn export_piece_ranges_respects_piece_and_chunk_caps() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let nine_mib = "a".repeat(9 * 1024 * 1024);
        let one_mib = "b".repeat(1024 * 1024);

        // Batch of [1 MiB, 1 MiB, 9 MiB, 1 MiB] with 8 MiB / 64 MiB caps:
        // Piece 0: rows 0..2 (~2 MiB); Piece 1: row 2..3 (~9 MiB, single row > 8 MiB allowed);
        // Piece 2: row 3..4 (~1 MiB).
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices([
                        one_mib.as_str(),
                        one_mib.as_str(),
                        nine_mib.as_str(),
                        one_mib.as_str(),
                    ])
                    .unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 4).unwrap();
        let ranges =
            export_piece_ranges(&batch, EXPORT_PIECE_BYTES, EXPORT_MAX_CHUNK_BYTES).unwrap();
        assert_eq!(ranges, vec![0..2, 2..3, 3..4]);

        // A single row > 64 MiB fails with TooLarge.
        let huge = "x".repeat(EXPORT_MAX_CHUNK_BYTES + 1);
        let huge_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([huge.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let huge_batch = Batch::try_new_all(schema, vec![huge_col], 1).unwrap();
        let err = export_piece_ranges(&huge_batch, EXPORT_PIECE_BYTES, EXPORT_MAX_CHUNK_BYTES)
            .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));

        // max_piece_bytes > max_chunk_bytes clamps piece_cap to max_chunk_bytes instead of
        // spuriously failing multi-row pieces with TooLarge.
        let small = "s".repeat(100);
        let small_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices([small.as_str(), small.as_str()]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let small_batch =
            Batch::try_new_all(Arc::clone(huge_batch.schema()), vec![small_col], 2).unwrap();
        let clamped = export_piece_ranges(&small_batch, 1024, 150).unwrap();
        assert_eq!(clamped, vec![0..1, 1..2]);
    }

    #[test]
    fn capped_buffer_drop_rolls_back_unless_disarmed_and_batch_sizes_use_output_schema() {
        let scope = GlobalPool::standalone(256);
        let mut res = scope.register("drop_test", false);
        res.try_grow(10).unwrap();

        {
            let mut buf = CappedBuffer::new(200, Some(&mut res));
            buf.push_bytes(b"hello").unwrap();
            assert_eq!(buf.len(), 5);
        }
        // Dropped without into_bytes -> rolled back to `before` (10).
        assert_eq!(res.size(), 10);

        let mut buf2 = CappedBuffer::new(200, Some(&mut res));
        buf2.push_bytes(b"world").unwrap();
        let bytes = buf2.into_bytes();
        assert_eq!(bytes, b"world");
        // Disarmed by into_bytes -> reservation stays at 15.
        assert_eq!(res.size(), 15);

        let batch_schema =
            Schema::try_new_arc(vec![Field::new("a", DataType::I64, false)]).unwrap();
        let aliased_schema =
            Schema::try_new_arc(vec![Field::new("very_long_alias", DataType::I64, false)]).unwrap();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap());
        let batch = Batch::try_new_all(batch_schema, vec![col], 1).unwrap();
        let batch_sz = batch_row_sizes_prefix(batch.schema(), &batch, 1).unwrap();
        let alias_sz = batch_row_sizes_prefix(&aliased_schema, &batch, 1).unwrap();
        assert_eq!(
            alias_sz[0] - batch_sz[0],
            ("very_long_alias".len() - "a".len()) as u64
        );
    }
}
