//! Direct `cetra-column` result encoders for SQL console JSON, CSV, and NDJSON.
//!
//! All encoders format values directly from `cetra_column::Batch` and its four
//! [`Column`] encodings (`Flat`, `Const`, `Dict`, `Rle`) while honoring [`Selection`]
//! vectors without cloning batches.

use std::fmt::{self, Write as _};
use std::ops::Range;

use cetra_column::{Batch, Column, DataType, Interval, ScalarValue, Schema, Selection};
use chrono::{DateTime, Datelike, Timelike};

use crate::error::QueryError;
use crate::eval::{
    EventsRef, LinksRef, ListRef, MapRef, events_ref_at, format_decimal, links_ref_at, list_ref_at,
    map_ref_at, read_bool, read_bytes, read_date, read_decimal, read_f64, read_i64, read_interval,
    read_str, read_timestamp,
};
use crate::fn_time::format_date_days;

pub mod csv;
pub mod json;
pub mod ndjson;
pub mod size;

pub use csv::{encode_csv_chunk, encode_csv_header};
pub use json::{SqlJsonResult, encode_json_result};
pub use ndjson::encode_ndjson_chunk;
pub use size::{CappedBuffer, batch_row_sizes, column_row_sizes, export_piece_ranges};

/// Maximum rows a SQL console JSON response may return.
pub const SQL_MAX_ROWS: usize = 10_000;

/// Maximum encoded JSON bytes a SQL console response may occupy (32 MiB).
pub const SQL_MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum data rows a streaming export may emit.
pub const EXPORT_MAX_ROWS: usize = 10_000_000;

/// Target estimated size for each streaming export batch slice (8 MiB).
pub const EXPORT_PIECE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum allowed bytes for a single streaming export chunk (64 MiB).
pub const EXPORT_MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;

const _: () = assert!(EXPORT_PIECE_BYTES <= EXPORT_MAX_CHUNK_BYTES);

const SCRATCH_FLUSH_BYTES: usize = 4096;
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Formats a nanosecond timestamp as fixed-9-digit RFC 3339 (`%Y-%m-%dT%H:%M:%S%.9fZ` when
/// `utc` is true, or `%Y-%m-%dT%H:%M:%S%.9f` when `utc` is false), never trimming trailing zeros.
pub fn format_timestamp_ns_fixed9(ns: i64, utc: bool) -> String {
    let mut out = String::with_capacity(if utc { 30 } else { 29 });
    let _ = write_timestamp_ns_fixed9(ns, utc, &mut out);
    out
}

pub(crate) fn write_timestamp_ns_fixed9(
    ns: i64,
    utc: bool,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    let dt = DateTime::from_timestamp_nanos(ns);
    write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.nanosecond()
    )?;
    if utc {
        out.write_char('Z')?;
    }
    Ok(())
}

/// Formats an [`Interval`] into its canonical SQL display string (at most 76 bytes).
pub fn format_interval(iv: Interval) -> String {
    let mut out = String::with_capacity(32);
    let _ = write_interval(iv, &mut out);
    out
}

pub(crate) fn write_interval(iv: Interval, out: &mut impl fmt::Write) -> fmt::Result {
    if iv.months == 0 && iv.days == 0 && iv.nanos == 0 {
        return out.write_str("0 secs");
    }

    let mut prefix = "";
    if iv.months != 0 {
        write!(out, "{prefix}{} mons", iv.months)?;
        prefix = " ";
    }
    if iv.days != 0 {
        write!(out, "{prefix}{} days", iv.days)?;
        prefix = " ";
    }
    if iv.nanos != 0 {
        let secs = iv.nanos / 1_000_000_000;
        let mins = secs / 60;
        let hours = mins / 60;
        let rem_secs = secs - (mins * 60);
        let rem_mins = mins - (hours * 60);
        let rem_nanos = iv.nanos % 1_000_000_000;

        if hours != 0 {
            write!(out, "{prefix}{hours} hours")?;
            prefix = " ";
        }
        if rem_mins != 0 {
            write!(out, "{prefix}{rem_mins} mins")?;
            prefix = " ";
        }
        if rem_secs != 0 || rem_nanos != 0 {
            let sign = if rem_secs < 0 || rem_nanos < 0 {
                "-"
            } else {
                ""
            };
            write!(
                out,
                "{prefix}{sign}{}.{:09} secs",
                rem_secs.unsigned_abs(),
                rem_nanos.unsigned_abs()
            )?;
        }
    }
    Ok(())
}

pub(crate) fn for_each_active_row(
    batch: &Batch,
    row_range: Range<usize>,
    mut f: impl FnMut(usize) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    if row_range.start > row_range.end || row_range.end > batch.num_rows() {
        return Err(QueryError::internal("row range out of bounds"));
    }
    if row_range.is_empty() {
        return Ok(());
    }
    match batch.selection() {
        Selection::All => {
            for phys in row_range {
                f(phys)?;
            }
        }
        Selection::Indices(idx) => {
            for &phys in &idx[row_range] {
                f(phys as usize)?;
            }
        }
        Selection::Mask(bm) => {
            for phys in bm
                .iter_ones()
                .skip(row_range.start)
                .take(row_range.end - row_range.start)
            {
                f(phys)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn verify_batch_schema(schema: &Schema, batch: &Batch) -> Result<(), QueryError> {
    if batch.schema().len() != schema.len() {
        return Err(QueryError::internal(
            "encoded batch does not match output schema",
        ));
    }
    for (expected, actual) in schema.fields().iter().zip(batch.schema().fields()) {
        if expected.data_type != actual.data_type {
            return Err(QueryError::internal(
                "encoded batch column type does not match output schema",
            ));
        }
    }
    Ok(())
}

pub(crate) fn encode_json_keys(schema: &Schema) -> Result<Vec<Vec<u8>>, QueryError> {
    let mut keys = Vec::with_capacity(schema.len());
    for field in schema.fields() {
        let mut encoded = Vec::with_capacity(field.name.len() + 3);
        serde_json::to_writer(&mut encoded, &field.name)
            .map_err(|_| QueryError::internal("failed to encode JSON column name"))?;
        encoded.push(b':');
        keys.push(encoded);
    }
    Ok(keys)
}

/// Chunk-buffered writer over [`CappedBuffer`] that amortizes [`Reservation::try_grow`] calls
/// while bounding unreserved scratch memory to ~4 KiB.
pub(crate) struct BufferedSink<'a, 'b> {
    sink: &'a mut CappedBuffer<'b>,
    buf: Vec<u8>,
    fmt_scratch: String,
    num_scratch: Vec<u8>,
}

impl<'a, 'b> BufferedSink<'a, 'b> {
    pub(crate) fn new(sink: &'a mut CappedBuffer<'b>) -> Self {
        Self {
            sink,
            buf: Vec::with_capacity(SCRATCH_FLUSH_BYTES),
            fmt_scratch: String::with_capacity(128),
            num_scratch: Vec::with_capacity(32),
        }
    }

    pub(crate) fn push_byte(&mut self, b: u8) -> Result<(), QueryError> {
        if self.buf.len() >= SCRATCH_FLUSH_BYTES {
            self.flush()?;
        }
        self.buf.push(b);
        Ok(())
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), QueryError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.buf.len().saturating_add(bytes.len()) >= SCRATCH_FLUSH_BYTES {
            self.flush()?;
            if bytes.len() >= SCRATCH_FLUSH_BYTES {
                return self.sink.push_bytes(bytes);
            }
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn push_str_formatted(
        &mut self,
        f: impl FnOnce(&mut String) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        self.fmt_scratch.clear();
        f(&mut self.fmt_scratch)?;
        let len = self.fmt_scratch.len();
        if self.buf.len().saturating_add(len) >= SCRATCH_FLUSH_BYTES {
            self.flush()?;
            if len >= SCRATCH_FLUSH_BYTES {
                return self.sink.push_bytes(self.fmt_scratch.as_bytes());
            }
        }
        self.buf.extend_from_slice(self.fmt_scratch.as_bytes());
        Ok(())
    }

    pub(crate) fn push_json_f64(&mut self, v: f64) -> Result<(), QueryError> {
        if !v.is_finite() {
            return self.push_bytes(b"null");
        }
        self.num_scratch.clear();
        serde_json::to_writer(&mut self.num_scratch, &v)
            .map_err(|_| QueryError::internal("failed to encode f64"))?;
        if std::str::from_utf8(&self.num_scratch).is_err() {
            return Err(QueryError::internal("non-utf8 f64 formatting"));
        }
        if self.buf.len().saturating_add(self.num_scratch.len()) >= SCRATCH_FLUSH_BYTES {
            self.flush()?;
            if self.num_scratch.len() >= SCRATCH_FLUSH_BYTES {
                return self.sink.push_bytes(&self.num_scratch);
            }
        }
        self.buf.extend_from_slice(&self.num_scratch);
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> Result<(), QueryError> {
        if !self.buf.is_empty() {
            self.sink.push_bytes(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    pub(crate) fn sink_mut(&mut self) -> &mut CappedBuffer<'b> {
        &mut *self.sink
    }
}

pub(crate) fn write_json_str(s: &str, out: &mut BufferedSink<'_, '_>) -> Result<(), QueryError> {
    out.push_byte(b'"')?;
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x0c => b"\\f",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x00..=0x1f => {
                if i > start {
                    out.push_bytes(&bytes[start..i])?;
                }
                let seq = [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX_LOWER[(b >> 4) as usize],
                    HEX_LOWER[(b & 0x0f) as usize],
                ];
                out.push_bytes(&seq)?;
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        if i > start {
            out.push_bytes(&bytes[start..i])?;
        }
        out.push_bytes(esc)?;
        start = i + 1;
    }
    if start < bytes.len() {
        out.push_bytes(&bytes[start..])?;
    }
    out.push_byte(b'"')
}

pub(crate) fn write_json_hex(
    bytes: &[u8],
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    out.push_byte(b'"')?;
    let mut chunk = [0u8; 512];
    for piece in bytes.chunks(chunk.len() / 2) {
        let mut pos = 0usize;
        for &b in piece {
            chunk[pos] = HEX_LOWER[(b >> 4) as usize];
            chunk[pos + 1] = HEX_LOWER[(b & 0x0f) as usize];
            pos += 2;
        }
        out.push_bytes(&chunk[..pos])?;
    }
    out.push_byte(b'"')
}

pub(crate) fn write_hex_to_string(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_LOWER[(b >> 4) as usize] as char);
        out.push(HEX_LOWER[(b & 0x0f) as usize] as char);
    }
}

pub(crate) fn write_json_f64(v: f64, out: &mut BufferedSink<'_, '_>) -> Result<(), QueryError> {
    out.push_json_f64(v)
}

pub(crate) fn write_json_i64(v: i64, out: &mut BufferedSink<'_, '_>) -> Result<(), QueryError> {
    out.push_str_formatted(|s| {
        let _ = write!(s, "{v}");
        Ok(())
    })
}

fn write_json_event(
    time: i64,
    name: &str,
    attrs: Option<&str>,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    out.push_bytes(b"{\"time\":\"")?;
    out.push_str_formatted(|s| {
        write_timestamp_ns_fixed9(time, true, s)
            .map_err(|_| QueryError::internal("failed to format event timestamp"))
    })?;
    out.push_bytes(b"\",\"name\":")?;
    write_json_str(name, out)?;
    out.push_bytes(b",\"attributes\":")?;
    match attrs {
        Some(a) => write_json_str(a, out)?,
        None => out.push_bytes(b"null")?,
    }
    out.push_byte(b'}')
}

fn write_json_link(
    trace_id: &[u8; 16],
    span_id: &[u8; 8],
    attrs: Option<&str>,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    out.push_bytes(b"{\"trace_id\":")?;
    write_json_hex(trace_id, out)?;
    out.push_bytes(b",\"span_id\":")?;
    write_json_hex(span_id, out)?;
    out.push_bytes(b",\"attributes\":")?;
    match attrs {
        Some(a) => write_json_str(a, out)?,
        None => out.push_bytes(b"null")?,
    }
    out.push_byte(b'}')
}

pub(crate) fn write_json_row(
    batch: &Batch,
    phys_row: usize,
    encoded_keys: &[Vec<u8>],
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    out.push_byte(b'{')?;
    for (idx, (key, col)) in encoded_keys.iter().zip(batch.columns()).enumerate() {
        if idx > 0 {
            out.push_byte(b',')?;
        }
        out.push_bytes(key)?;
        write_json_cell(col, phys_row, out)?;
    }
    out.push_byte(b'}')
}

pub(crate) fn write_json_cell(
    col: &Column,
    phys_row: usize,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    if col.is_null(phys_row) {
        return out.push_bytes(b"null");
    }
    match col.data_type() {
        DataType::Null => out.push_bytes(b"null"),
        DataType::Bool => {
            let v = read_bool(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing bool cell"))?;
            out.push_bytes(if v { b"true" } else { b"false" })
        }
        DataType::I64 => {
            let v =
                read_i64(col, phys_row).ok_or_else(|| QueryError::internal("missing i64 cell"))?;
            write_json_i64(v, out)
        }
        DataType::F64 => {
            let v =
                read_f64(col, phys_row).ok_or_else(|| QueryError::internal("missing f64 cell"))?;
            write_json_f64(v, out)
        }
        DataType::Decimal128 { scale, .. } => {
            let v = read_decimal(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing decimal cell"))?;
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                s.push_str(&format_decimal(v, scale));
                Ok(())
            })?;
            out.push_byte(b'"')
        }
        DataType::TimestampNs { utc } => {
            let v = read_timestamp(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing timestamp cell"))?;
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                write_timestamp_ns_fixed9(v, utc, s)
                    .map_err(|_| QueryError::internal("failed to format timestamp"))
            })?;
            out.push_byte(b'"')
        }
        DataType::Date => {
            let v = read_date(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing date cell"))?;
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                s.push_str(&format_date_days(v));
                Ok(())
            })?;
            out.push_byte(b'"')
        }
        DataType::Interval => {
            let iv = read_interval(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing interval cell"))?;
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                write_interval(iv, s).map_err(|_| QueryError::internal("failed to format interval"))
            })?;
            out.push_byte(b'"')
        }
        DataType::Utf8 => {
            let s =
                read_str(col, phys_row).ok_or_else(|| QueryError::internal("missing utf8 cell"))?;
            write_json_str(s, out)
        }
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            let b = read_bytes(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing binary cell"))?;
            write_json_hex(b, out)
        }
        DataType::List(_) => {
            let list_ref = list_ref_at(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing list cell"))?;
            out.push_byte(b'[')?;
            match list_ref {
                ListRef::Flat { values, start, end } => {
                    for idx in start..end {
                        if idx > start {
                            out.push_byte(b',')?;
                        }
                        write_json_cell(values, idx, out)?;
                    }
                }
                ListRef::Const(items) => {
                    for (idx, sv) in items.iter().enumerate() {
                        if idx > 0 {
                            out.push_byte(b',')?;
                        }
                        write_json_scalar(sv, out)?;
                    }
                }
            }
            out.push_byte(b']')
        }
        DataType::MapUtf8 => {
            let map_ref = map_ref_at(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing map cell"))?;
            out.push_byte(b'{')?;
            match map_ref {
                MapRef::Flat { data, start, end } => {
                    for idx in start..end {
                        if idx > start {
                            out.push_byte(b',')?;
                        }
                        let key = data
                            .keys()
                            .get(idx)
                            .ok_or_else(|| QueryError::internal("map key out of bounds"))?;
                        write_json_str(key, out)?;
                        out.push_byte(b':')?;
                        if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                            out.push_bytes(b"null")?;
                        } else {
                            let val = data
                                .values()
                                .get(idx)
                                .ok_or_else(|| QueryError::internal("map value out of bounds"))?;
                            write_json_str(val, out)?;
                        }
                    }
                }
                MapRef::Const(entries) => {
                    for (idx, (k, v)) in entries.iter().enumerate() {
                        if idx > 0 {
                            out.push_byte(b',')?;
                        }
                        write_json_str(k, out)?;
                        out.push_byte(b':')?;
                        match v {
                            Some(val) => write_json_str(val, out)?,
                            None => out.push_bytes(b"null")?,
                        }
                    }
                }
            }
            out.push_byte(b'}')
        }
        DataType::Events => {
            let events_ref = events_ref_at(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing events cell"))?;
            out.push_byte(b'[')?;
            match events_ref {
                EventsRef::Flat { data, start, end } => {
                    for idx in start..end {
                        if idx > start {
                            out.push_byte(b',')?;
                        }
                        let time = data
                            .times()
                            .get(idx)
                            .copied()
                            .ok_or_else(|| QueryError::internal("event time out of bounds"))?;
                        let name = data
                            .names()
                            .get(idx)
                            .ok_or_else(|| QueryError::internal("event name out of bounds"))?;
                        let attrs =
                            if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                                None
                            } else {
                                Some(data.attributes().get(idx).ok_or_else(|| {
                                    QueryError::internal("event attrs out of bounds")
                                })?)
                            };
                        write_json_event(time, name, attrs, out)?;
                    }
                }
                EventsRef::Const(items) => {
                    for (idx, ev) in items.iter().enumerate() {
                        if idx > 0 {
                            out.push_byte(b',')?;
                        }
                        write_json_event(ev.time, &ev.name, ev.attributes.as_deref(), out)?;
                    }
                }
            }
            out.push_byte(b']')
        }
        DataType::Links => {
            let links_ref = links_ref_at(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing links cell"))?;
            out.push_byte(b'[')?;
            match links_ref {
                LinksRef::Flat { data, start, end } => {
                    for idx in start..end {
                        if idx > start {
                            out.push_byte(b',')?;
                        }
                        let tid = data
                            .trace_ids()
                            .get(idx)
                            .ok_or_else(|| QueryError::internal("link trace_id out of bounds"))?;
                        let sid = data
                            .span_ids()
                            .get(idx)
                            .ok_or_else(|| QueryError::internal("link span_id out of bounds"))?;
                        let attrs =
                            if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                                None
                            } else {
                                Some(data.attributes().get(idx).ok_or_else(|| {
                                    QueryError::internal("link attrs out of bounds")
                                })?)
                            };
                        write_json_link(tid, sid, attrs, out)?;
                    }
                }
                LinksRef::Const(items) => {
                    for (idx, lk) in items.iter().enumerate() {
                        if idx > 0 {
                            out.push_byte(b',')?;
                        }
                        write_json_link(&lk.trace_id, &lk.span_id, lk.attributes.as_deref(), out)?;
                    }
                }
            }
            out.push_byte(b']')
        }
    }
}

pub(crate) fn write_json_scalar(
    sv: &ScalarValue,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    match sv {
        ScalarValue::Null(_) => out.push_bytes(b"null"),
        ScalarValue::Bool(b) => out.push_bytes(if *b { b"true" } else { b"false" }),
        ScalarValue::I64(v) => write_json_i64(*v, out),
        ScalarValue::F64(v) => write_json_f64(*v, out),
        ScalarValue::Decimal128 { value, scale, .. } => {
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                s.push_str(&format_decimal(*value, *scale));
                Ok(())
            })?;
            out.push_byte(b'"')
        }
        ScalarValue::TimestampNs { value, utc } => {
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                write_timestamp_ns_fixed9(*value, *utc, s)
                    .map_err(|_| QueryError::internal("failed to format timestamp"))
            })?;
            out.push_byte(b'"')
        }
        ScalarValue::Date(days) => {
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                s.push_str(&format_date_days(*days));
                Ok(())
            })?;
            out.push_byte(b'"')
        }
        ScalarValue::Interval(iv) => {
            out.push_byte(b'"')?;
            out.push_str_formatted(|s| {
                write_interval(*iv, s)
                    .map_err(|_| QueryError::internal("failed to format interval"))
            })?;
            out.push_byte(b'"')
        }
        ScalarValue::Utf8(s) => write_json_str(s, out),
        ScalarValue::Binary(b) => write_json_hex(b, out),
        ScalarValue::FixedBytes8(b) => write_json_hex(b, out),
        ScalarValue::FixedBytes16(b) => write_json_hex(b, out),
        ScalarValue::List { values, .. } => {
            out.push_byte(b'[')?;
            for (idx, item) in values.iter().enumerate() {
                if idx > 0 {
                    out.push_byte(b',')?;
                }
                write_json_scalar(item, out)?;
            }
            out.push_byte(b']')
        }
        ScalarValue::MapUtf8(entries) => {
            out.push_byte(b'{')?;
            for (idx, (k, v)) in entries.iter().enumerate() {
                if idx > 0 {
                    out.push_byte(b',')?;
                }
                write_json_str(k, out)?;
                out.push_byte(b':')?;
                match v {
                    Some(val) => write_json_str(val, out)?,
                    None => out.push_bytes(b"null")?,
                }
            }
            out.push_byte(b'}')
        }
        ScalarValue::Events(items) => {
            out.push_byte(b'[')?;
            for (idx, ev) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_byte(b',')?;
                }
                write_json_event(ev.time, &ev.name, ev.attributes.as_deref(), out)?;
            }
            out.push_byte(b']')
        }
        ScalarValue::Links(items) => {
            out.push_byte(b'[')?;
            for (idx, lk) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_byte(b',')?;
                }
                write_json_link(&lk.trace_id, &lk.span_id, lk.attributes.as_deref(), out)?;
            }
            out.push_byte(b']')
        }
    }
}

/// Encodes a single `Batch` column cell (`List`, `MapUtf8`, `Events`, `Links`, or any nested value)
/// to a compact JSON UTF-8 slice bounded by `out`'s remaining byte cap and `Reservation`, passing
/// it to `write_csv` while the temporary JSON buffer's reservation remains held.
pub(crate) fn write_cell_json_to_csv_sink(
    col: &Column,
    phys_row: usize,
    out: &mut BufferedSink<'_, '_>,
    write_csv: impl FnOnce(&str, &mut BufferedSink<'_, '_>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    out.flush()?;
    out.sink_mut().with_temp(
        |temp_sink| {
            let mut temp_out = BufferedSink::new(temp_sink);
            write_json_cell(col, phys_row, &mut temp_out)?;
            temp_out.flush()
        },
        |temp_bytes, outer_sink| {
            let text = std::str::from_utf8(temp_bytes)
                .map_err(|_| QueryError::internal("non-utf8 JSON cell"))?;
            let mut outer_out = BufferedSink::new(outer_sink);
            write_csv(text, &mut outer_out)?;
            outer_out.flush()
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        BinaryData, Bitmap, Column, DataType, DictColumn, EventItem, Field, FlatColumn, FlatData,
        Interval, LinksData, MapData, RleColumn, ScalarValue, Schema, Selection, Utf8Data,
    };

    use super::*;

    #[test]
    fn constants_match_spec() {
        assert_eq!(SQL_MAX_ROWS, 10_000);
        assert_eq!(SQL_MAX_RESULT_BYTES, 32 * 1024 * 1024);
        assert_eq!(EXPORT_MAX_ROWS, 10_000_000);
        assert_eq!(EXPORT_PIECE_BYTES, 8 * 1024 * 1024);
        assert_eq!(EXPORT_MAX_CHUNK_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn timestamp_fixed9_never_trims_trailing_zeros() {
        assert_eq!(
            format_timestamp_ns_fixed9(0, true),
            "1970-01-01T00:00:00.000000000Z"
        );
        assert_eq!(
            format_timestamp_ns_fixed9(0, false),
            "1970-01-01T00:00:00.000000000"
        );
        assert_eq!(
            format_timestamp_ns_fixed9(1_230_000_000, true),
            "1970-01-01T00:00:01.230000000Z"
        );
        assert_eq!(
            format_timestamp_ns_fixed9(-1, true),
            "1969-12-31T23:59:59.999999999Z"
        );
        assert_eq!(
            format_timestamp_ns_fixed9(-1, false),
            "1969-12-31T23:59:59.999999999"
        );
    }

    #[test]
    fn interval_formatting_matches_canonical_bounds() {
        assert_eq!(format_interval(Interval::default()), "0 secs");
        assert_eq!(
            format_interval(Interval {
                months: 14,
                days: -2,
                nanos: 3_661_000_000_500,
            }),
            "14 mons -2 days 1 hours 1 mins 1.000000500 secs"
        );
        let min_str = format_interval(Interval {
            months: i32::MIN,
            days: i32::MIN,
            nanos: i64::MIN,
        });
        assert!(min_str.len() <= 94, "too long: {min_str}");
        let max_str = format_interval(Interval {
            months: i32::MAX,
            days: i32::MAX,
            nanos: i64::MAX,
        });
        assert!(max_str.len() <= 94, "too long: {max_str}");
    }

    #[test]
    fn json_string_escaping_matches_serde_json() {
        let mut all = String::new();
        for b in 0u8..=127u8 {
            all.push(b as char);
        }
        all.push_str("héllo 🚀\u{2028}");

        let mut sink = CappedBuffer::new(usize::MAX, None);
        let mut out = BufferedSink::new(&mut sink);
        write_json_str(&all, &mut out).unwrap();
        out.flush().unwrap();
        let actual = String::from_utf8(sink.into_bytes()).unwrap();
        let expected = serde_json::to_string(&all).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn all_data_types_four_encodings_and_selection_round_trip() {
        let schema = Schema::try_new_arc(vec![
            Field::new("null_c", DataType::Null, true),
            Field::new("bool_c", DataType::Bool, true),
            Field::new("i64_c", DataType::I64, true),
            Field::new("f64_c", DataType::F64, true),
            Field::new(
                "dec_c",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            Field::new("ts_utc", DataType::TimestampNs { utc: true }, true),
            Field::new("ts_naive", DataType::TimestampNs { utc: false }, true),
            Field::new("date_c", DataType::Date, true),
            Field::new("iv_c", DataType::Interval, true),
            Field::new("utf8_dict", DataType::Utf8, true),
            Field::new("bin_c", DataType::Binary, true),
            Field::new("fb8_c", DataType::FixedBytes8, true),
            Field::new("fb16_c", DataType::FixedBytes16, true),
            Field::new("list_rle", DataType::List(Box::new(DataType::I64)), true),
            Field::new("map_c", DataType::MapUtf8, true),
            Field::new("ev_const", DataType::Events, true),
            Field::new("lk_c", DataType::Links, true),
        ])
        .unwrap();

        // 3 physical rows; Selection::Indices([2, 0]) reverses and filters out row 1.
        let null_c = Column::Flat(FlatColumn::try_new(FlatData::Null(3), None).unwrap());
        let bool_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(&[true, false, false])),
                Some(Bitmap::from_bools(&[true, true, false])),
            )
            .unwrap(),
        );
        let i64_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![42, 99, -7]),
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let f64_c = Column::Flat(
            FlatColumn::try_new(FlatData::F64(vec![f64::NAN, 1.5, f64::INFINITY]), None).unwrap(),
        );
        let dec_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![1234, 0, -5],
                    precision: 10,
                    scale: 2,
                },
                None,
            )
            .unwrap(),
        );
        let ts_utc = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![0, 1, 1_500_000_000],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let ts_naive = Column::Const {
            value: ScalarValue::TimestampNs {
                value: 0,
                utc: false,
            },
            len: 3,
        };
        let date_c =
            Column::Flat(FlatColumn::try_new(FlatData::Date(vec![0, 1, -719_529]), None).unwrap());
        let iv_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    Interval::default(),
                    Interval::default(),
                    Interval {
                        months: -1,
                        days: 2,
                        nanos: -500,
                    },
                ]),
                None,
            )
            .unwrap(),
        );
        let utf8_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["hello", "=1+1"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 1, 1],
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let bin_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices([b"\xde\xad".as_slice(), b"", b"\x00\xff"])
                        .unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let fb8_c = Column::Const {
            value: ScalarValue::FixedBytes8([1, 2, 3, 4, 5, 6, 7, 8]),
            len: 3,
        };
        let fb16_c = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![[0u8; 16], [1u8; 16], [0xab; 16]]),
                None,
            )
            .unwrap(),
        );

        let list_elems = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![10, 0, 20, 30]),
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        let list_values = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::I64,
                offsets: vec![0, 2, 4],
                values: Box::new(list_elems),
            },
            Some(Bitmap::from_bools(&[true, true])),
        )
        .unwrap();
        let list_rle = Column::Rle(RleColumn::try_new(list_values, vec![2, 3]).unwrap());

        let map_data = MapData::try_new(
            vec![0, 2, 2, 3],
            Utf8Data::try_from_slices(["k1", "k2", "only"]).unwrap(),
            Utf8Data::try_from_slices(["v1", "", "val"]).unwrap(),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let map_c =
            Column::Flat(FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None).unwrap());

        let ev_const = Column::Const {
            value: ScalarValue::Events(vec![EventItem {
                time: 0,
                name: "ev".to_owned(),
                attributes: None,
            }]),
            len: 3,
        };

        let lk_data = LinksData::try_new(
            vec![0, 1, 1, 2],
            vec![[0xaa; 16], [0xbb; 16]],
            vec![[0x11; 8], [0x22; 8]],
            Utf8Data::try_from_slices(["", "{\"a\":1}"]).unwrap(),
            Some(Bitmap::from_bools(&[false, true])),
        )
        .unwrap();
        let lk_c =
            Column::Flat(FlatColumn::try_new(FlatData::Links(Box::new(lk_data)), None).unwrap());

        let batch = Batch::try_new(
            Arc::clone(&schema),
            vec![
                null_c, bool_c, i64_c, f64_c, dec_c, ts_utc, ts_naive, date_c, iv_c, utf8_dict,
                bin_c, fb8_c, fb16_c, list_rle, map_c, ev_const, lk_c,
            ],
            3,
            Selection::Indices(vec![2, 0]),
        )
        .unwrap();

        // Check estimate >= actual_json_bytes for NDJSON.
        let estimates = batch_row_sizes(&batch).unwrap();
        assert_eq!(estimates.len(), 2);
        let ndjson_bytes =
            encode_ndjson_chunk(&batch, 0..batch.num_rows(), usize::MAX, None).unwrap();
        let ndjson_str = String::from_utf8(ndjson_bytes).unwrap();
        let lines: Vec<&str> = ndjson_str.lines().collect();
        assert_eq!(lines.len(), 2);
        for (i, line) in lines.iter().enumerate() {
            assert!(
                estimates[i] >= line.len() as u64,
                "row {i}: estimate {} < actual {}",
                estimates[i],
                line.len()
            );
        }

        // Check JSON result decoding, explicit nulls, NaN/Inf -> null, Decimal128 string.
        let res = encode_json_result(
            &schema,
            std::slice::from_ref(&batch),
            10,
            usize::MAX,
            false,
            None,
        )
        .unwrap();
        assert!(!res.truncated);
        assert_eq!(res.rows.len(), 2);
        // Active row 0 is physical row 2:
        let r0 = &res.rows[0];
        assert_eq!(r0["null_c"], serde_json::Value::Null);
        assert_eq!(r0["bool_c"], serde_json::Value::Null);
        assert_eq!(r0["i64_c"], serde_json::json!(-7));
        assert_eq!(r0["f64_c"], serde_json::Value::Null); // +inf -> null
        assert_eq!(r0["dec_c"], serde_json::json!("-0.05"));
        assert_eq!(
            r0["ts_utc"],
            serde_json::json!("1970-01-01T00:00:01.500000000Z")
        );
        assert_eq!(
            r0["ts_naive"],
            serde_json::json!("1970-01-01T00:00:00.000000000")
        );
        assert_eq!(r0["date_c"], serde_json::json!("-0001-12-31"));
        assert_eq!(
            r0["iv_c"],
            serde_json::json!("-1 mons 2 days -0.000000500 secs")
        );
        assert_eq!(r0["utf8_dict"], serde_json::json!("=1+1"));
        assert_eq!(r0["bin_c"], serde_json::json!("00ff"));
        assert_eq!(r0["fb8_c"], serde_json::json!("0102030405060708"));
        assert_eq!(
            r0["fb16_c"],
            serde_json::json!("abababababababababababababababab")
        );
        assert_eq!(r0["list_rle"], serde_json::json!([20, 30]));
        assert_eq!(r0["map_c"], serde_json::json!({"only": "val"}));
        assert_eq!(
            r0["ev_const"],
            serde_json::json!([{
                "time": "1970-01-01T00:00:00.000000000Z",
                "name": "ev",
                "attributes": null
            }])
        );
        assert_eq!(
            r0["lk_c"],
            serde_json::json!([{
                "trace_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "span_id": "2222222222222222",
                "attributes": "{\"a\":1}"
            }])
        );

        // Active row 1 is physical row 0: explicit nulls inside MapUtf8, List, and Links.
        let r1 = &res.rows[1];
        assert_eq!(r1["f64_c"], serde_json::Value::Null); // NaN -> null
        assert_eq!(r1["dec_c"], serde_json::json!("12.34"));
        assert_eq!(r1["list_rle"], serde_json::json!([10, null]));
        assert_eq!(r1["map_c"], serde_json::json!({"k1": "v1", "k2": null}));
        assert_eq!(
            r1["lk_c"],
            serde_json::json!([{
                "trace_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "span_id": "1111111111111111",
                "attributes": null
            }])
        );
    }
}
