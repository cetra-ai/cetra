//! Versioned, CRC-32C-checksummed, LZ4-compressed, DoS-bounded binary batch framing.

use std::io::{Read, Write};
use std::ops::Range;

use lz4_flex::frame::{FrameDecoder, FrameEncoder};

use crate::column::{reconcile_reservation, rollback_reservation};
use crate::{
    Batch, BinaryData, Bitmap, Column, ColumnError, DataType, DictColumn, EventsData, Field,
    FlatColumn, FlatData, Interval, LinksData, MAX_NESTING_DEPTH, MapData, Reservation, RleColumn,
    ScalarValue, Schema, Utf8Data,
};

/// 4-byte magic prefix of a v1 batch frame.
pub const FRAME_MAGIC: [u8; 4] = *b"CBM1";
/// Current binary batch frame format version.
pub const FRAME_VERSION: u8 = 1;
/// Fixed header length in bytes:
/// `magic(4) + version(1) + flags(1) + reserved(2) + uncompressed_len(4) + payload_len(4) + crc32c(4)`.
pub const FRAME_HEADER_LEN: usize = 20;
/// Flag bit indicating `payload` is an LZ4 frame.
pub const FLAG_LZ4: u8 = 0x01;
/// Maximum encoded payload length (256 MiB, matching `wal/record.rs`).
pub const MAX_PAYLOAD: u32 = 256 * 1024 * 1024;
/// Maximum decompressed body length (1 GiB, matching `wal/record.rs`).
pub const MAX_DECODED_BYTES: u32 = 1 << 30;
/// Maximum allowed LZ4 expansion ratio before rejecting as a bomb (matching `wal/record.rs`).
pub const LZ4_MAX_RATIO: u64 = 256;
/// Fixed slack bytes for LZ4 frame headers in the bomb check (matching `wal/record.rs`).
pub const LZ4_SLACK: u64 = 64;
/// Maximum number of columns in one framed batch (guards hostile column counts).
pub const MAX_COLUMNS: u32 = 1024;

const LZ4_MIN_COMPRESS_LEN: usize = 64;

/// One step of streaming frame scanning.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameSlice {
    /// Complete checksum-verified frame slice inside the buffer.
    Frame {
        /// Byte range of the full frame (header + payload) in `buf`.
        frame: Range<usize>,
        /// Offset of the next frame.
        next: usize,
    },
    /// `offset == buf.len()`.
    End,
    /// Incomplete header or payload at the tail of `buf` (torn write).
    Short {
        /// Offset where the incomplete frame begins.
        offset: usize,
    },
    /// Corrupt magic, version, flags, lengths, or CRC-32C checksum.
    Invalid {
        /// Offset where the bad frame begins.
        offset: usize,
        /// Static reason (never payload bytes).
        reason: &'static str,
    },
}

struct ParsedHeader {
    flags: u8,
    uncompressed_len: u32,
    payload_len: u32,
    crc32c: u32,
    total_len: usize,
}

fn validate_header_fields(hdr: &[u8]) -> Result<ParsedHeader, &'static str> {
    if hdr[0..4] != FRAME_MAGIC {
        return Err("invalid frame magic");
    }
    if hdr[4] != FRAME_VERSION {
        return Err("unsupported frame version");
    }
    let flags = hdr[5];
    if flags & !FLAG_LZ4 != 0 {
        return Err("unknown frame flags");
    }
    let reserved = u16::from_le_bytes([hdr[6], hdr[7]]);
    if reserved != 0 {
        return Err("non-zero reserved header bytes");
    }
    let uncompressed_len = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let payload_len = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
    if payload_len > MAX_PAYLOAD {
        return Err("payload length exceeds MAX_PAYLOAD");
    }
    if uncompressed_len > MAX_DECODED_BYTES {
        return Err("uncompressed length exceeds MAX_DECODED_BYTES");
    }
    if flags == 0 && payload_len != uncompressed_len {
        return Err("uncompressed frame payload_len differs from uncompressed_len");
    }
    let Some(total_len) = FRAME_HEADER_LEN.checked_add(payload_len as usize) else {
        return Err("frame total length overflows usize");
    };
    let crc32c = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    Ok(ParsedHeader {
        flags,
        uncompressed_len,
        payload_len,
        crc32c,
        total_len,
    })
}

/// Encodes `batch` (compacting active rows first if `!selection.is_all(raw_len)`, preserving column encodings
/// when all rows are active) and appends one frame to `out`. On error, `out` is left unchanged.
pub fn encode_batch(batch: &Batch, out: &mut Vec<u8>) -> Result<(), ColumnError> {
    encode_batch_with_limit(batch, out, MAX_DECODED_BYTES)
}

/// [`encode_batch`] with a custom decoded-byte cap (for testing small caps).
pub fn encode_batch_with_limit(
    batch: &Batch,
    out: &mut Vec<u8>,
    max_decoded: u32,
) -> Result<(), ColumnError> {
    if batch.schema().len() > MAX_COLUMNS as usize {
        return Err(ColumnError::InvalidFrame {
            reason: "column count exceeds MAX_COLUMNS",
        });
    }
    if batch.num_rows() > u32::MAX as usize {
        return Err(ColumnError::Overflow {
            reason: "batch row count exceeds u32::MAX",
        });
    }

    let compacted_owner;
    let target = if batch.selection().is_all(batch.raw_len()) {
        batch
    } else {
        compacted_owner = batch.compact(None)?;
        &compacted_owner
    };

    let mut body = Vec::new();
    write_body(target, &mut body)?;

    if body.len() > max_decoded as usize || body.len() > MAX_DECODED_BYTES as usize {
        return Err(ColumnError::InvalidFrame {
            reason: "uncompressed body exceeds maximum decoded bytes",
        });
    }
    let uncompressed_len = u32::try_from(body.len()).map_err(|_| ColumnError::Overflow {
        reason: "uncompressed body length exceeds u32::MAX",
    })?;

    let compressed;
    let (flags, payload): (u8, &[u8]) = if body.len() >= LZ4_MIN_COMPRESS_LEN {
        let mut encoder = FrameEncoder::new(Vec::new());
        encoder
            .write_all(&body)
            .map_err(|_| ColumnError::InvalidFrame {
                reason: "lz4 compression failed",
            })?;
        compressed = encoder.finish().map_err(|_| ColumnError::InvalidFrame {
            reason: "lz4 compression finish failed",
        })?;
        if compressed.len() < body.len() {
            (FLAG_LZ4, &compressed)
        } else {
            (0, &body)
        }
    } else {
        (0, &body)
    };

    let payload_len = u32::try_from(payload.len()).map_err(|_| ColumnError::Overflow {
        reason: "payload length exceeds u32::MAX",
    })?;
    if payload_len > MAX_PAYLOAD {
        return Err(ColumnError::InvalidFrame {
            reason: "encoded payload exceeds MAX_PAYLOAD",
        });
    }

    let mut header = [0u8; FRAME_HEADER_LEN];
    header[0..4].copy_from_slice(&FRAME_MAGIC);
    header[4] = FRAME_VERSION;
    header[5] = flags;
    header[6..8].copy_from_slice(&0u16.to_le_bytes());
    header[8..12].copy_from_slice(&uncompressed_len.to_le_bytes());
    header[12..16].copy_from_slice(&payload_len.to_le_bytes());
    let crc = crc32c::crc32c_append(crc32c::crc32c(&header[0..16]), payload);
    header[16..20].copy_from_slice(&crc.to_le_bytes());

    out.reserve(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    Ok(())
}

/// Scans the next frame starting at `offset` in `buf`, checking header invariants and CRC-32C without decompressing.
pub fn decode_next(buf: &[u8], offset: usize) -> FrameSlice {
    if offset == buf.len() {
        return FrameSlice::End;
    }
    if offset > buf.len() || buf.len() - offset < FRAME_HEADER_LEN {
        return FrameSlice::Short { offset };
    }
    let hdr = &buf[offset..offset + FRAME_HEADER_LEN];
    let parsed = match validate_header_fields(hdr) {
        Ok(p) => p,
        Err(reason) => return FrameSlice::Invalid { offset, reason },
    };
    if buf.len() - offset < parsed.total_len {
        return FrameSlice::Short { offset };
    }
    let payload = &buf[offset + FRAME_HEADER_LEN..offset + parsed.total_len];
    let actual_crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), payload);
    if actual_crc != parsed.crc32c {
        return FrameSlice::Invalid {
            offset,
            reason: "checksum mismatch",
        };
    }
    let next = offset + parsed.total_len;
    FrameSlice::Frame {
        frame: offset..next,
        next,
    }
}

/// Decodes one complete frame (header + payload) into a [`Batch`], reserving from `pool`.
/// Postcondition: on `Ok(batch)`, `pool.size()` increased by `batch.memory_bytes()` (temporary decompressed
/// body buffer is released before returning); on `Err`, `pool.size()` is restored to its pre-call value.
/// When `expected_schema` is `Some`, rejects frames whose decoded schema differs.
pub fn decode_batch(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    pool: Option<&mut Reservation>,
) -> Result<Batch, ColumnError> {
    decode_batch_with_limit(frame, expected_schema, pool, MAX_DECODED_BYTES)
}

/// [`decode_batch`] with a custom decoded-byte cap (for testing small caps).
pub fn decode_batch_with_limit(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
) -> Result<Batch, ColumnError> {
    let before = pool.as_ref().map_or(0, |p| p.size());
    match decode_batch_inner(frame, expected_schema, pool.as_deref_mut(), max_decoded) {
        Ok(batch) => {
            reconcile_reservation(pool, before, batch.memory_bytes())?;
            Ok(batch)
        }
        Err(err) => {
            rollback_reservation(pool, before);
            Err(err)
        }
    }
}

fn decode_batch_inner(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
) -> Result<Batch, ColumnError> {
    if frame.len() < FRAME_HEADER_LEN {
        return Err(ColumnError::InvalidFrame {
            reason: "frame shorter than header",
        });
    }
    let hdr = &frame[..FRAME_HEADER_LEN];
    let parsed =
        validate_header_fields(hdr).map_err(|reason| ColumnError::InvalidFrame { reason })?;
    if parsed.uncompressed_len > max_decoded {
        return Err(ColumnError::InvalidFrame {
            reason: "uncompressed length exceeds max_decoded",
        });
    }
    if frame.len() != parsed.total_len {
        return Err(ColumnError::InvalidFrame {
            reason: "frame length does not match header payload_len",
        });
    }
    let payload = &frame[FRAME_HEADER_LEN..parsed.total_len];
    let actual_crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), payload);
    if actual_crc != parsed.crc32c {
        return Err(ColumnError::InvalidFrame {
            reason: "checksum mismatch",
        });
    }

    let decoded_owner: Vec<u8>;
    let body: &[u8] = if parsed.flags & FLAG_LZ4 != 0 {
        let max_allowed = u64::from(parsed.payload_len)
            .saturating_mul(LZ4_MAX_RATIO)
            .saturating_add(LZ4_SLACK);
        if u64::from(parsed.uncompressed_len) > max_allowed {
            return Err(ColumnError::InvalidFrame {
                reason: "lz4 expansion ratio exceeds limit",
            });
        }
        let scratch_len =
            usize::try_from(parsed.uncompressed_len).map_err(|_| ColumnError::Overflow {
                reason: "uncompressed length overflows usize",
            })?;
        if let Some(res) = pool.as_deref_mut() {
            res.try_grow(scratch_len)?;
        }
        let limit = u64::from(parsed.uncompressed_len) + 1;
        let mut decoder = FrameDecoder::new(payload).take(limit);
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .map_err(|_| ColumnError::InvalidFrame {
                reason: "lz4 decompression failed",
            })?;
        if buf.len() != scratch_len {
            return Err(ColumnError::InvalidFrame {
                reason: "lz4 decoded length does not match header",
            });
        }
        decoded_owner = buf;
        &decoded_owner
    } else {
        payload
    };

    let mut reader = BodyReader::new(body);
    let raw_len = reader.read_u32()? as usize;
    let col_count = reader.read_u32()?;
    if col_count > MAX_COLUMNS {
        return Err(ColumnError::InvalidFrame {
            reason: "column count exceeds MAX_COLUMNS",
        });
    }
    let col_count = col_count as usize;

    let mut fields = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let name_len = reader.read_u32()? as usize;
        let name_bytes = reader.read_bytes(name_len)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ColumnError::InvalidUtf8)?
            .to_owned();
        if name.is_empty() {
            return Err(ColumnError::InvalidFrame {
                reason: "field name cannot be empty",
            });
        }
        let nullable = reader.read_bool_u8()?;
        let data_type = read_data_type(&mut reader, 0)?;
        fields.push(Field::new(name, data_type, nullable));
    }
    let schema = Schema::try_new_arc(fields)?;

    if let Some(expected) = expected_schema
        && schema.as_ref() != expected
    {
        return Err(ColumnError::InvalidFrame {
            reason: "decoded schema does not match expected schema",
        });
    }

    let mut columns = Vec::with_capacity(col_count);
    for field in schema.fields() {
        let col = read_column(
            &mut reader,
            &field.data_type,
            raw_len,
            0,
            pool.as_deref_mut(),
        )?;
        columns.push(col);
    }

    if reader.remaining() != 0 {
        return Err(ColumnError::InvalidFrame {
            reason: "trailing bytes after batch body",
        });
    }

    Batch::try_new_all(schema, columns, raw_len)
}

fn checked_u32(val: usize, reason: &'static str) -> Result<u32, ColumnError> {
    u32::try_from(val).map_err(|_| ColumnError::Overflow { reason })
}

fn write_bool_u8(out: &mut Vec<u8>, b: bool) {
    out.push(u8::from(b));
}

fn write_data_type(out: &mut Vec<u8>, dt: &DataType) {
    match dt {
        DataType::Null => out.push(0),
        DataType::Bool => out.push(1),
        DataType::I64 => out.push(2),
        DataType::F64 => out.push(3),
        DataType::Decimal128 { precision, scale } => {
            out.push(4);
            out.push(*precision);
            out.push(*scale as u8);
        }
        DataType::TimestampNs { utc } => {
            out.push(5);
            write_bool_u8(out, *utc);
        }
        DataType::Date => out.push(6),
        DataType::Interval => out.push(7),
        DataType::Utf8 => out.push(8),
        DataType::Binary => out.push(9),
        DataType::FixedBytes8 => out.push(10),
        DataType::FixedBytes16 => out.push(11),
        DataType::List(inner) => {
            out.push(12);
            write_data_type(out, inner);
        }
        DataType::MapUtf8 => out.push(13),
        DataType::Events => out.push(14),
        DataType::Links => out.push(15),
    }
}

fn write_u32_slice(out: &mut Vec<u8>, slice: &[u32]) {
    out.reserve(slice.len() * 4);
    for &v in slice {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn write_utf8_data(out: &mut Vec<u8>, u: &Utf8Data) -> Result<(), ColumnError> {
    write_u32_slice(out, u.offsets());
    let bytes_len = checked_u32(u.bytes().len(), "utf8 bytes length exceeds u32::MAX")?;
    out.extend_from_slice(&bytes_len.to_le_bytes());
    out.extend_from_slice(u.bytes());
    Ok(())
}

fn write_binary_data(out: &mut Vec<u8>, b: &BinaryData) -> Result<(), ColumnError> {
    write_u32_slice(out, b.offsets());
    let bytes_len = checked_u32(b.bytes().len(), "binary bytes length exceeds u32::MAX")?;
    out.extend_from_slice(&bytes_len.to_le_bytes());
    out.extend_from_slice(b.bytes());
    Ok(())
}

fn write_optional_bitmap(out: &mut Vec<u8>, bm: Option<&Bitmap>) {
    match bm {
        Some(b) => {
            out.push(1);
            out.extend_from_slice(&b.to_bytes());
        }
        None => out.push(0),
    }
}

fn write_flat_data(out: &mut Vec<u8>, data: &FlatData) -> Result<(), ColumnError> {
    match data {
        FlatData::Null(_) => {}
        FlatData::Bool(bm) => {
            out.extend_from_slice(&bm.to_bytes());
        }
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
            out.reserve(vals.len() * 8);
            for &v in vals {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        FlatData::F64(vals) => {
            out.reserve(vals.len() * 8);
            for &v in vals {
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        }
        FlatData::Decimal128 { values, .. } => {
            out.reserve(values.len() * 16);
            for &v in values {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        FlatData::Date(vals) => {
            out.reserve(vals.len() * 4);
            for &v in vals {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        FlatData::Interval(vals) => {
            out.reserve(vals.len() * 16);
            for &iv in vals {
                out.extend_from_slice(&iv.months.to_le_bytes());
                out.extend_from_slice(&iv.days.to_le_bytes());
                out.extend_from_slice(&iv.nanos.to_le_bytes());
            }
        }
        FlatData::Utf8(u) => write_utf8_data(out, u)?,
        FlatData::Binary(b) => write_binary_data(out, b)?,
        FlatData::FixedBytes8(vals) => {
            out.reserve(vals.len() * 8);
            for item in vals {
                out.extend_from_slice(item);
            }
        }
        FlatData::FixedBytes16(vals) => {
            out.reserve(vals.len() * 16);
            for item in vals {
                out.extend_from_slice(item);
            }
        }
        FlatData::List {
            offsets, values, ..
        } => {
            write_u32_slice(out, offsets);
            let child_len = checked_u32(values.len(), "list child length exceeds u32::MAX")?;
            out.extend_from_slice(&child_len.to_le_bytes());
            write_column(out, values)?;
        }
        FlatData::MapUtf8(m) => {
            write_u32_slice(out, m.offsets());
            let entry_count = checked_u32(m.entry_count(), "map entry count exceeds u32::MAX")?;
            out.extend_from_slice(&entry_count.to_le_bytes());
            write_utf8_data(out, m.keys())?;
            write_utf8_data(out, m.values())?;
            write_optional_bitmap(out, m.value_nulls());
        }
        FlatData::Events(e) => {
            write_u32_slice(out, e.offsets());
            let ev_count = checked_u32(e.event_count(), "events count exceeds u32::MAX")?;
            out.extend_from_slice(&ev_count.to_le_bytes());
            out.reserve(e.times().len() * 8);
            for &t in e.times() {
                out.extend_from_slice(&t.to_le_bytes());
            }
            write_utf8_data(out, e.names())?;
            write_utf8_data(out, e.attributes())?;
            write_optional_bitmap(out, e.attribute_nulls());
        }
        FlatData::Links(l) => {
            write_u32_slice(out, l.offsets());
            let lk_count = checked_u32(l.link_count(), "links count exceeds u32::MAX")?;
            out.extend_from_slice(&lk_count.to_le_bytes());
            out.reserve(l.trace_ids().len() * 16 + l.span_ids().len() * 8);
            for tid in l.trace_ids() {
                out.extend_from_slice(tid);
            }
            for sid in l.span_ids() {
                out.extend_from_slice(sid);
            }
            write_utf8_data(out, l.attributes())?;
            write_optional_bitmap(out, l.attribute_nulls());
        }
    }
    Ok(())
}

fn write_flat_column(out: &mut Vec<u8>, fc: &FlatColumn) -> Result<(), ColumnError> {
    write_optional_bitmap(out, fc.nulls());
    write_flat_data(out, fc.data())
}

fn write_column(out: &mut Vec<u8>, col: &Column) -> Result<(), ColumnError> {
    match col {
        Column::Flat(fc) => {
            out.push(0);
            write_flat_column(out, fc)?;
        }
        Column::Const { value, .. } => {
            out.push(1);
            if value.is_null() {
                out.push(1);
            } else {
                out.push(0);
                let one = Column::try_new_const(value.clone(), 1)?.materialize(None)?;
                write_flat_data(out, one.data())?;
            }
        }
        Column::Dict(d) => {
            out.push(2);
            let dict_len = checked_u32(d.values().len(), "dict length exceeds u32::MAX")?;
            out.extend_from_slice(&dict_len.to_le_bytes());
            write_flat_column(out, d.values())?;
            write_u32_slice(out, d.codes());
            write_optional_bitmap(out, d.nulls());
        }
        Column::Rle(r) => {
            out.push(3);
            let run_count = checked_u32(r.values().len(), "rle run count exceeds u32::MAX")?;
            out.extend_from_slice(&run_count.to_le_bytes());
            write_flat_column(out, r.values())?;
            write_u32_slice(out, r.run_ends());
        }
    }
    Ok(())
}

fn write_body(batch: &Batch, out: &mut Vec<u8>) -> Result<(), ColumnError> {
    let raw_len = checked_u32(batch.raw_len(), "raw_len exceeds u32::MAX")?;
    let col_count = checked_u32(batch.schema().len(), "column count exceeds u32::MAX")?;
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&col_count.to_le_bytes());

    for field in batch.schema().fields() {
        let name_len = checked_u32(field.name.len(), "field name length exceeds u32::MAX")?;
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(field.name.as_bytes());
        write_bool_u8(out, field.nullable);
        write_data_type(out, &field.data_type);
    }

    for col in batch.columns() {
        write_column(out, col)?;
    }
    Ok(())
}

struct BodyReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], ColumnError> {
        if len > self.remaining() {
            return Err(ColumnError::InvalidFrame {
                reason: "unexpected end of batch frame body",
            });
        }
        let end = self.pos + len;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, ColumnError> {
        let bytes = self.read_bytes(1)?;
        Ok(bytes[0])
    }

    fn read_bool_u8(&mut self) -> Result<bool, ColumnError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ColumnError::InvalidFrame {
                reason: "invalid boolean byte in frame",
            }),
        }
    }

    fn read_u32(&mut self) -> Result<u32, ColumnError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn checked_byte_len(count: usize, elem_size: usize) -> Result<usize, ColumnError> {
        count.checked_mul(elem_size).ok_or(ColumnError::Overflow {
            reason: "element byte length overflow",
        })
    }

    fn reserve_checked(
        &self,
        byte_len: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<(), ColumnError> {
        if byte_len > self.remaining() {
            return Err(ColumnError::InvalidFrame {
                reason: "unexpected end of batch frame body",
            });
        }
        if let Some(res) = pool {
            res.try_grow(byte_len)?;
        }
        Ok(())
    }

    fn read_chunks<const N: usize, T>(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
        convert: impl Fn([u8; N]) -> T,
    ) -> Result<Vec<T>, ColumnError> {
        let byte_len = Self::checked_byte_len(count, N)?;
        self.reserve_checked(byte_len, pool)?;
        let bytes = self.read_bytes(byte_len)?;
        let (chunks, _) = bytes.as_chunks::<N>();
        let mut out = Vec::with_capacity(count);
        for &chunk in chunks {
            out.push(convert(chunk));
        }
        Ok(out)
    }

    fn read_u32_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<u32>, ColumnError> {
        self.read_chunks::<4, u32>(count, pool, u32::from_le_bytes)
    }

    fn read_i32_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<i32>, ColumnError> {
        self.read_chunks::<4, i32>(count, pool, i32::from_le_bytes)
    }

    fn read_i64_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<i64>, ColumnError> {
        self.read_chunks::<8, i64>(count, pool, i64::from_le_bytes)
    }

    fn read_f64_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<f64>, ColumnError> {
        self.read_chunks::<8, f64>(count, pool, |b| f64::from_bits(u64::from_le_bytes(b)))
    }

    fn read_i128_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<i128>, ColumnError> {
        self.read_chunks::<16, i128>(count, pool, i128::from_le_bytes)
    }

    fn read_interval_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<Interval>, ColumnError> {
        self.read_chunks::<16, Interval>(count, pool, |chunk| {
            let months = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let days = i32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let nanos = i64::from_le_bytes([
                chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14],
                chunk[15],
            ]);
            Interval {
                months,
                days,
                nanos,
            }
        })
    }

    fn read_fixed8_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<[u8; 8]>, ColumnError> {
        self.read_chunks::<8, [u8; 8]>(count, pool, |b| b)
    }

    fn read_fixed16_vec(
        &mut self,
        count: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<[u8; 16]>, ColumnError> {
        self.read_chunks::<16, [u8; 16]>(count, pool, |b| b)
    }

    fn read_offsets(
        &mut self,
        len: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Vec<u32>, ColumnError> {
        let count = len.checked_add(1).ok_or(ColumnError::Overflow {
            reason: "offsets count overflow",
        })?;
        self.read_u32_vec(count, pool)
    }

    fn read_bitmap(
        &mut self,
        len: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Bitmap, ColumnError> {
        let wire_len = len.div_ceil(8);
        if wire_len > self.remaining() {
            return Err(ColumnError::InvalidFrame {
                reason: "unexpected end of batch frame body",
            });
        }
        if let Some(res) = pool {
            let mem_bytes = len
                .div_ceil(64)
                .checked_mul(8)
                .ok_or(ColumnError::Overflow {
                    reason: "bitmap memory length overflow",
                })?;
            res.try_grow(mem_bytes)?;
        }
        let bytes = self.read_bytes(wire_len)?;
        Bitmap::from_bytes(bytes, len)
    }

    fn read_optional_bitmap(
        &mut self,
        len: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Option<Bitmap>, ColumnError> {
        if self.read_bool_u8()? {
            Ok(Some(self.read_bitmap(len, pool)?))
        } else {
            Ok(None)
        }
    }

    fn read_utf8_data(
        &mut self,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Utf8Data, ColumnError> {
        let offsets = self.read_offsets(len, pool.as_deref_mut())?;
        let bytes_len = self.read_u32()? as usize;
        self.reserve_checked(bytes_len, pool)?;
        let bytes = self.read_bytes(bytes_len)?.to_vec();
        Utf8Data::try_new(offsets, bytes)
    }

    fn read_binary_data(
        &mut self,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<BinaryData, ColumnError> {
        let offsets = self.read_offsets(len, pool.as_deref_mut())?;
        let bytes_len = self.read_u32()? as usize;
        self.reserve_checked(bytes_len, pool)?;
        let bytes = self.read_bytes(bytes_len)?.to_vec();
        BinaryData::try_new(offsets, bytes)
    }
}

fn read_data_type(reader: &mut BodyReader<'_>, depth: usize) -> Result<DataType, ColumnError> {
    let tag = reader.read_u8()?;
    match tag {
        0 => Ok(DataType::Null),
        1 => Ok(DataType::Bool),
        2 => Ok(DataType::I64),
        3 => Ok(DataType::F64),
        4 => {
            let precision = reader.read_u8()?;
            let scale = reader.read_u8()? as i8;
            let dt = DataType::Decimal128 { precision, scale };
            dt.validate(MAX_NESTING_DEPTH)
                .map_err(|_| ColumnError::InvalidFrame {
                    reason: "invalid decimal precision or scale in frame",
                })?;
            Ok(dt)
        }
        5 => {
            let utc = reader.read_bool_u8()?;
            Ok(DataType::TimestampNs { utc })
        }
        6 => Ok(DataType::Date),
        7 => Ok(DataType::Interval),
        8 => Ok(DataType::Utf8),
        9 => Ok(DataType::Binary),
        10 => Ok(DataType::FixedBytes8),
        11 => Ok(DataType::FixedBytes16),
        12 => {
            if depth >= MAX_NESTING_DEPTH {
                return Err(ColumnError::InvalidFrame {
                    reason: "list nesting depth exceeds maximum",
                });
            }
            let inner = read_data_type(reader, depth + 1)?;
            Ok(DataType::List(Box::new(inner)))
        }
        13 => Ok(DataType::MapUtf8),
        14 => Ok(DataType::Events),
        15 => Ok(DataType::Links),
        _ => Err(ColumnError::InvalidFrame {
            reason: "unknown data type tag",
        }),
    }
}

fn read_flat_data(
    reader: &mut BodyReader<'_>,
    data_type: &DataType,
    len: usize,
    depth: usize,
    mut pool: Option<&mut Reservation>,
) -> Result<FlatData, ColumnError> {
    let data = match data_type {
        DataType::Null => FlatData::Null(len),
        DataType::Bool => FlatData::Bool(reader.read_bitmap(len, pool)?),
        DataType::I64 => FlatData::I64(reader.read_i64_vec(len, pool)?),
        DataType::F64 => FlatData::F64(reader.read_f64_vec(len, pool)?),
        DataType::Decimal128 { precision, scale } => FlatData::Decimal128 {
            values: reader.read_i128_vec(len, pool)?,
            precision: *precision,
            scale: *scale,
        },
        DataType::TimestampNs { utc } => FlatData::TimestampNs {
            values: reader.read_i64_vec(len, pool)?,
            utc: *utc,
        },
        DataType::Date => FlatData::Date(reader.read_i32_vec(len, pool)?),
        DataType::Interval => FlatData::Interval(reader.read_interval_vec(len, pool)?),
        DataType::Utf8 => FlatData::Utf8(reader.read_utf8_data(len, pool)?),
        DataType::Binary => FlatData::Binary(reader.read_binary_data(len, pool)?),
        DataType::FixedBytes8 => FlatData::FixedBytes8(reader.read_fixed8_vec(len, pool)?),
        DataType::FixedBytes16 => FlatData::FixedBytes16(reader.read_fixed16_vec(len, pool)?),
        DataType::List(inner) => {
            if depth >= MAX_NESTING_DEPTH {
                return Err(ColumnError::InvalidFrame {
                    reason: "list nesting depth exceeds maximum",
                });
            }
            let offsets = reader.read_offsets(len, pool.as_deref_mut())?;
            let child_len = reader.read_u32()? as usize;
            let values = read_column(reader, inner, child_len, depth + 1, pool)?;
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(values),
            }
        }
        DataType::MapUtf8 => {
            let offsets = reader.read_offsets(len, pool.as_deref_mut())?;
            let entry_count = reader.read_u32()? as usize;
            let keys = reader.read_utf8_data(entry_count, pool.as_deref_mut())?;
            let values = reader.read_utf8_data(entry_count, pool.as_deref_mut())?;
            let value_nulls = reader.read_optional_bitmap(entry_count, pool)?;
            FlatData::MapUtf8(Box::new(MapData::try_new(
                offsets,
                keys,
                values,
                value_nulls,
            )?))
        }
        DataType::Events => {
            let offsets = reader.read_offsets(len, pool.as_deref_mut())?;
            let ev_count = reader.read_u32()? as usize;
            let times = reader.read_i64_vec(ev_count, pool.as_deref_mut())?;
            let names = reader.read_utf8_data(ev_count, pool.as_deref_mut())?;
            let attributes = reader.read_utf8_data(ev_count, pool.as_deref_mut())?;
            let attribute_nulls = reader.read_optional_bitmap(ev_count, pool)?;
            FlatData::Events(Box::new(EventsData::try_new(
                offsets,
                times,
                names,
                attributes,
                attribute_nulls,
            )?))
        }
        DataType::Links => {
            let offsets = reader.read_offsets(len, pool.as_deref_mut())?;
            let lk_count = reader.read_u32()? as usize;
            let trace_ids = reader.read_fixed16_vec(lk_count, pool.as_deref_mut())?;
            let span_ids = reader.read_fixed8_vec(lk_count, pool.as_deref_mut())?;
            let attributes = reader.read_utf8_data(lk_count, pool.as_deref_mut())?;
            let attribute_nulls = reader.read_optional_bitmap(lk_count, pool)?;
            FlatData::Links(Box::new(LinksData::try_new(
                offsets,
                trace_ids,
                span_ids,
                attributes,
                attribute_nulls,
            )?))
        }
    };
    Ok(data)
}

fn read_flat_column(
    reader: &mut BodyReader<'_>,
    data_type: &DataType,
    len: usize,
    depth: usize,
    mut pool: Option<&mut Reservation>,
) -> Result<FlatColumn, ColumnError> {
    let has_nulls = reader.read_bool_u8()?;
    if has_nulls && matches!(data_type, DataType::Null) {
        return Err(ColumnError::InvalidFrame {
            reason: "null data type cannot carry nulls bitmap",
        });
    }
    let nulls = if has_nulls {
        Some(reader.read_bitmap(len, pool.as_deref_mut())?)
    } else {
        None
    };
    let data = read_flat_data(reader, data_type, len, depth, pool)?;
    FlatColumn::try_new(data, nulls)
}

fn read_column(
    reader: &mut BodyReader<'_>,
    data_type: &DataType,
    len: usize,
    depth: usize,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, ColumnError> {
    let encoding_tag = reader.read_u8()?;
    match encoding_tag {
        0 => Ok(Column::Flat(read_flat_column(
            reader, data_type, len, depth, pool,
        )?)),
        1 => {
            let is_null = reader.read_bool_u8()?;
            if is_null {
                Column::try_new_const(ScalarValue::Null(data_type.clone()), len)
            } else {
                if matches!(data_type, DataType::Null) {
                    return Err(ColumnError::InvalidFrame {
                        reason: "const column with DataType::Null must have is_null = 1",
                    });
                }
                let data = read_flat_data(reader, data_type, 1, depth, pool)?;
                let flat = FlatColumn::try_new(data, None)?;
                let scalar = flat.get_scalar(0)?;
                Column::try_new_const(scalar, len)
            }
        }
        2 => {
            let dict_len = reader.read_u32()? as usize;
            let values = read_flat_column(reader, data_type, dict_len, depth, pool.as_deref_mut())?;
            let codes = reader.read_u32_vec(len, pool.as_deref_mut())?;
            let nulls = reader.read_optional_bitmap(len, pool)?;
            Ok(Column::Dict(DictColumn::try_new(values, codes, nulls)?))
        }
        3 => {
            let run_count = reader.read_u32()? as usize;
            let values =
                read_flat_column(reader, data_type, run_count, depth, pool.as_deref_mut())?;
            let run_ends = reader.read_u32_vec(run_count, pool)?;
            let rle = RleColumn::try_new(values, run_ends)?;
            if rle.len() != len {
                return Err(ColumnError::InvalidFrame {
                    reason: "rle column length does not match expected length",
                });
            }
            Ok(Column::Rle(rle))
        }
        _ => Err(ColumnError::InvalidFrame {
            reason: "unknown column encoding tag",
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use crate::{
        COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, EventItem, GlobalPool, LinkItem,
        MemoryLimitKind, Selection,
    };

    use super::*;

    fn all_types_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("null_col", DataType::Null, true),
            Field::new("bool_col", DataType::Bool, true),
            Field::new("i64_col", DataType::I64, true),
            Field::new("f64_col", DataType::F64, true),
            Field::new("dec_col", DataType::cost_decimal(), true),
            Field::new("ts_col", DataType::timestamp_utc(), true),
            Field::new("ts_naive_col", DataType::TimestampNs { utc: false }, true),
            Field::new("date_col", DataType::Date, true),
            Field::new("iv_col", DataType::Interval, true),
            Field::new("utf8_col", DataType::Utf8, true),
            Field::new("bin_col", DataType::Binary, true),
            Field::new("fb8_col", DataType::FixedBytes8, true),
            Field::new("fb16_col", DataType::FixedBytes16, true),
            Field::new(
                "nested_list_col",
                DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
                true,
            ),
            Field::new("map_col", DataType::MapUtf8, true),
            Field::new("events_col", DataType::Events, true),
            Field::new("links_col", DataType::Links, true),
            Field::new("const_str", DataType::Utf8, false),
            Field::new("const_list", DataType::List(Box::new(DataType::I64)), false),
            Field::new("dict_utf8", DataType::Utf8, true),
            Field::new("rle_i64", DataType::I64, true),
        ])
        .unwrap()
    }

    fn all_types_batch() -> Batch {
        let schema = all_types_schema();
        let n = 4usize;
        let nulls = Some(Bitmap::from_bools(&[true, false, true, true]));

        let c_null = Column::Flat(FlatColumn::try_new(FlatData::Null(n), None).unwrap());
        let c_bool = Column::Flat(
            FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(&[true, false, false, true])),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_i64 = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 0, -5, 42]), nulls.clone()).unwrap(),
        );
        let c_f64 = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64(vec![1.25, 0.0, f64::NAN, -0.0]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_dec = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![1_000_000_000_000_000_000, 0, -42, 999],
                    precision: COST_DECIMAL_PRECISION,
                    scale: COST_DECIMAL_SCALE,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_ts = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![1, 0, 2, 3],
                    utc: true,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_ts_naive = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![4, 0, 5, 6],
                    utc: false,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_date = Column::Flat(
            FlatColumn::try_new(FlatData::Date(vec![19_000, 0, -1, 20_000]), nulls.clone())
                .unwrap(),
        );
        let c_iv = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    Interval {
                        months: 1,
                        days: 2,
                        nanos: 3,
                    },
                    Interval::default(),
                    Interval {
                        months: -1,
                        days: -2,
                        nanos: -3,
                    },
                    Interval {
                        months: 0,
                        days: 7,
                        nanos: 0,
                    },
                ]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_utf8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["hello", "", "é🚀", "world"]).unwrap()),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_bin = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices([
                        &b"ab"[..],
                        &b""[..],
                        &b"\x00\xff"[..],
                        &b"z"[..],
                    ])
                    .unwrap(),
                ),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_fb8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes8(vec![[1; 8], [0; 8], [2; 8], [3; 8]]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_fb16 = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![[4; 16], [0; 16], [5; 16], [6; 16]]),
                nulls.clone(),
            )
            .unwrap(),
        );

        // Nested List(List(Utf8))
        let leaf_utf8 = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["x", "", "y", "z"]).unwrap()),
            Some(Bitmap::from_bools(&[true, false, true, true])),
        )
        .unwrap();
        let inner_list = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 2, 2, 4],
                values: Box::new(Column::Flat(leaf_utf8)),
            },
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let c_nested_list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::List(Box::new(DataType::Utf8)),
                    offsets: vec![0, 1, 1, 2, 3],
                    values: Box::new(Column::Flat(inner_list)),
                },
                nulls.clone(),
            )
            .unwrap(),
        );

        let c_map = Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(
                    MapData::try_new(
                        vec![0, 2, 2, 3, 3],
                        Utf8Data::try_from_slices(["k1", "k2", "k3"]).unwrap(),
                        Utf8Data::try_from_slices(["v1", "", "v3"]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                )),
                nulls.clone(),
            )
            .unwrap(),
        );

        let c_events = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        vec![0, 1, 1, 2, 2],
                        vec![100, 200],
                        Utf8Data::try_from_slices(["ev1", "ev2"]).unwrap(),
                        Utf8Data::try_from_slices(["{}", ""]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                )),
                nulls.clone(),
            )
            .unwrap(),
        );

        let c_links = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        vec![0, 1, 1, 2, 2],
                        vec![[9; 16], [8; 16]],
                        vec![[7; 8], [6; 8]],
                        Utf8Data::try_from_slices(["", "{\"a\":1}"]).unwrap(),
                        Some(Bitmap::from_bools(&[false, true])),
                    )
                    .unwrap(),
                )),
                nulls,
            )
            .unwrap(),
        );

        let c_const_str =
            Column::try_new_const(ScalarValue::Utf8("constant-string-payload".into()), n).unwrap();
        let c_const_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: vec![
                    ScalarValue::I64(1),
                    ScalarValue::Null(DataType::I64),
                    ScalarValue::I64(3),
                ],
            },
            n,
        )
        .unwrap();

        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap();
        let c_dict = Column::Dict(
            DictColumn::try_new(
                dict_vals,
                vec![0, 1, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );

        let rle_vals = FlatColumn::try_new(
            FlatData::I64(vec![11, 0, 22]),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let c_rle = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 3, 4]).unwrap());

        Batch::try_new_all(
            schema,
            vec![
                c_null,
                c_bool,
                c_i64,
                c_f64,
                c_dec,
                c_ts,
                c_ts_naive,
                c_date,
                c_iv,
                c_utf8,
                c_bin,
                c_fb8,
                c_fb16,
                c_nested_list,
                c_map,
                c_events,
                c_links,
                c_const_str,
                c_const_list,
                c_dict,
                c_rle,
            ],
            n,
        )
        .unwrap()
    }

    #[test]
    fn round_trip_all_types_and_encodings() {
        let batch = all_types_batch();

        // Full batch round-trip preserves encodings
        let mut buf = Vec::new();
        encode_batch(&batch, &mut buf).unwrap();
        let decoded = decode_batch(&buf, Some(batch.schema()), None).unwrap();
        assert!(batch.semantic_eq(&decoded));
        assert!(matches!(decoded.columns()[17], Column::Const { .. }));
        assert!(matches!(decoded.columns()[18], Column::Const { .. }));
        assert!(matches!(decoded.columns()[19], Column::Dict(_)));
        assert!(matches!(decoded.columns()[20], Column::Rle(_)));

        // Active Selection::Mask is compacted on encode
        let filtered = batch
            .filter_mask(&Bitmap::from_bools(&[true, false, true, false]))
            .unwrap();
        let mut f_buf = Vec::new();
        encode_batch(&filtered, &mut f_buf).unwrap();
        let f_decoded = decode_batch(&f_buf, Some(filtered.schema()), None).unwrap();
        assert_eq!(f_decoded.num_rows(), 2);
        assert_eq!(f_decoded.raw_len(), 2);
        assert_eq!(f_decoded.selection(), &Selection::All);
        assert!(filtered.semantic_eq(&f_decoded));

        // Empty batch (0 rows) round-trip
        let empty = Batch::empty(all_types_schema());
        let mut e_buf = Vec::new();
        encode_batch(&empty, &mut e_buf).unwrap();
        let e_decoded = decode_batch(&e_buf, Some(empty.schema()), None).unwrap();
        assert!(empty.semantic_eq(&e_decoded));
    }

    #[test]
    fn multi_frame_stream_and_torn_tail() {
        let b1 = all_types_batch();
        let b2 = b1.slice(1, 2).unwrap();
        let b3 = Batch::empty(all_types_schema());

        let mut stream = Vec::new();
        encode_batch(&b1, &mut stream).unwrap();
        let f1_end = stream.len();
        encode_batch(&b2, &mut stream).unwrap();
        let f2_end = stream.len();
        encode_batch(&b3, &mut stream).unwrap();
        let f3_end = stream.len();

        let mut offset = 0;
        let mut decoded_batches = Vec::new();
        loop {
            match decode_next(&stream, offset) {
                FrameSlice::Frame { frame, next } => {
                    decoded_batches.push(decode_batch(&stream[frame], None, None).unwrap());
                    offset = next;
                }
                FrameSlice::End => break,
                other => panic!("unexpected frame slice: {other:?}"),
            }
        }
        assert_eq!(offset, f3_end);
        assert_eq!(decoded_batches.len(), 3);
        assert!(b1.semantic_eq(&decoded_batches[0]));
        assert!(b2.semantic_eq(&decoded_batches[1]));
        assert!(b3.semantic_eq(&decoded_batches[2]));

        // Exact boundary at end of 2nd frame -> End; every truncated byte position of 3rd frame -> Short
        assert_eq!(decode_next(&stream[..f2_end], f2_end), FrameSlice::End);
        for cut in (f2_end + 1)..f3_end {
            assert_eq!(
                decode_next(&stream[..cut], f2_end),
                FrameSlice::Short { offset: f2_end },
                "cut at {cut} (frame2 ended at {f2_end}, frame1 ended at {f1_end})"
            );
        }

        // Oversized corrupt header returns Invalid (not Short) even when truncated buffer is shorter than declared payload
        let mut bad = vec![0u8; FRAME_HEADER_LEN];
        bad[0..4].copy_from_slice(&FRAME_MAGIC);
        bad[4] = FRAME_VERSION;
        bad[5] = FLAG_LZ4;
        bad[8..12].copy_from_slice(&(MAX_DECODED_BYTES + 1).to_le_bytes());
        bad[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
    }

    #[test]
    fn corrupt_header_and_crc_rejected() {
        let batch = all_types_batch();
        let mut frame = Vec::new();
        encode_batch(&batch, &mut frame).unwrap();

        // 1. Bad magic
        let mut bad = frame.clone();
        bad[0] = b'X';
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 2. Bad version
        let mut bad = frame.clone();
        bad[4] = 99;
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 3. Unknown flags
        let mut bad = frame.clone();
        bad[5] = 0x02;
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 4. Non-zero reserved
        let mut bad = frame.clone();
        bad[6] = 1;
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 5. flags == 0 with mismatched lengths
        let mut bad = frame.clone();
        bad[5] = 0;
        bad[8..12].copy_from_slice(&100u32.to_le_bytes());
        bad[12..16].copy_from_slice(&50u32.to_le_bytes());
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid { offset: 0, .. }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 6. Flipped bit in header (uncompressed_len) -> CRC mismatch
        let mut bad = frame.clone();
        bad[8] ^= 0x01;
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid {
                offset: 0,
                reason: "checksum mismatch"
            }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame {
                reason: "checksum mismatch"
            })
        ));

        // 7. Flipped bit in payload -> CRC mismatch
        let mut bad = frame.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        assert!(matches!(
            decode_next(&bad, 0),
            FrameSlice::Invalid {
                offset: 0,
                reason: "checksum mismatch"
            }
        ));
        assert!(matches!(
            decode_batch(&bad, None, None),
            Err(ColumnError::InvalidFrame {
                reason: "checksum mismatch"
            })
        ));

        // Helper to wrap an arbitrary uncompressed body with valid CRC
        let wrap_uncompressed = |body: &[u8]| -> Vec<u8> {
            let mut f = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
            let mut hdr = [0u8; FRAME_HEADER_LEN];
            hdr[0..4].copy_from_slice(&FRAME_MAGIC);
            hdr[4] = FRAME_VERSION;
            hdr[5] = 0;
            let len_bytes = (body.len() as u32).to_le_bytes();
            hdr[8..12].copy_from_slice(&len_bytes);
            hdr[12..16].copy_from_slice(&len_bytes);
            let crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), body);
            hdr[16..20].copy_from_slice(&crc.to_le_bytes());
            f.extend_from_slice(&hdr);
            f.extend_from_slice(body);
            f
        };

        // 8. DataType::Null with Const { is_null: 0 }
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_le_bytes()); // raw_len = 3
        body.extend_from_slice(&1u32.to_le_bytes()); // col_count = 1
        body.extend_from_slice(&1u32.to_le_bytes()); // name_len = 1
        body.push(b'a');
        body.push(1); // nullable = true
        body.push(0); // DataType::Null
        body.push(1); // encoding = Const
        body.push(0); // is_null = 0 (invalid for Null)
        assert!(matches!(
            decode_batch(&wrap_uncompressed(&body), None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 9. Boolean flag > 1 (nullable = 2)
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body.push(b'a');
        body.push(2); // invalid boolean
        body.push(2); // DataType::I64
        assert!(matches!(
            decode_batch(&wrap_uncompressed(&body), None, None),
            Err(ColumnError::InvalidFrame { .. })
        ));

        // 10. Trailing byte after valid body
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let small = Batch::try_new_all(
            schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap(),
            )],
            2,
        )
        .unwrap();
        let mut small_body = Vec::new();
        write_body(&small, &mut small_body).unwrap();
        small_body.push(0xAA);
        assert!(matches!(
            decode_batch(&wrap_uncompressed(&small_body), None, None),
            Err(ColumnError::InvalidFrame {
                reason: "trailing bytes after batch body"
            })
        ));
    }

    #[test]
    fn lz4_bomb_and_size_caps_rejected() {
        // 1. Fabricated header with huge uncompressed_len and tiny payload fails bomb ratio before allocation
        let payload = [0u8; 8];
        let mut bomb = vec![0u8; FRAME_HEADER_LEN + payload.len()];
        bomb[0..4].copy_from_slice(&FRAME_MAGIC);
        bomb[4] = FRAME_VERSION;
        bomb[5] = FLAG_LZ4;
        bomb[8..12].copy_from_slice(&(100 * 1024 * 1024u32).to_le_bytes()); // 100 MiB declared
        bomb[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let crc = crc32c::crc32c_append(crc32c::crc32c(&bomb[0..16]), &payload);
        bomb[16..20].copy_from_slice(&crc.to_le_bytes());
        bomb[FRAME_HEADER_LEN..].copy_from_slice(&payload);

        let scope = GlobalPool::standalone(1024);
        let mut res = scope.register("bomb", true);
        let err = decode_batch(&bomb, None, Some(&mut res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "lz4 expansion ratio exceeds limit"
            }
        ));
        assert_eq!(res.size(), 0);

        // 2. Small max_decoded rejected on both encode and decode
        let batch = all_types_batch();
        let mut out = Vec::new();
        assert!(matches!(
            encode_batch_with_limit(&batch, &mut out, 16),
            Err(ColumnError::InvalidFrame { .. })
        ));
        assert!(out.is_empty());

        encode_batch(&batch, &mut out).unwrap();
        assert!(matches!(
            decode_batch_with_limit(&out, None, None, 16),
            Err(ColumnError::InvalidFrame {
                reason: "uncompressed length exceeds max_decoded"
            })
        ));

        // 3. Mismatched actual LZ4 output length rejected (valid ratio, valid CRC, wrong uncompressed_len)
        let real_body = vec![42u8; 100];
        let mut enc = FrameEncoder::new(Vec::new());
        enc.write_all(&real_body).unwrap();
        let comp = enc.finish().unwrap();

        let mut mismatch = Vec::with_capacity(FRAME_HEADER_LEN + comp.len());
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        hdr[0..4].copy_from_slice(&FRAME_MAGIC);
        hdr[4] = FRAME_VERSION;
        hdr[5] = FLAG_LZ4;
        hdr[8..12].copy_from_slice(&99u32.to_le_bytes()); // actual is 100
        hdr[12..16].copy_from_slice(&(comp.len() as u32).to_le_bytes());
        let crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), &comp);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        mismatch.extend_from_slice(&hdr);
        mismatch.extend_from_slice(&comp);

        assert!(matches!(
            decode_batch(&mismatch, None, None),
            Err(ColumnError::InvalidFrame {
                reason: "lz4 decoded length does not match header"
            })
        ));
    }

    #[test]
    fn expected_schema_mismatch_rejected() {
        let batch = all_types_batch();
        let mut buf = Vec::new();
        encode_batch(&batch, &mut buf).unwrap();

        let wrong_schema =
            Schema::try_new(vec![Field::new("other", DataType::I64, false)]).unwrap();
        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("schema_check", true);
        res.try_grow(64).unwrap();
        let err = decode_batch(&buf, Some(&wrong_schema), Some(&mut res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "decoded schema does not match expected schema"
            }
        ));
        assert_eq!(res.size(), 64);
    }

    #[test]
    fn decode_reservation_accounting_and_rollback() {
        let batch = all_types_batch();
        let mut frame = Vec::new();
        encode_batch(&batch, &mut frame).unwrap();

        let scope = GlobalPool::standalone(10 * 1024 * 1024);
        let mut res = scope.register("decode", true);
        res.try_grow(128).unwrap();
        let before = res.size();

        let decoded = decode_batch(&frame, Some(batch.schema()), Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + decoded.memory_bytes());

        // Tight reservation returns MemoryLimit and restores pre-call size
        let tight_scope = GlobalPool::standalone(256);
        let mut tight_res = tight_scope.register("tight", true);
        tight_res.try_grow(32).unwrap();
        let tight_before = tight_res.size();
        let err = decode_batch(&frame, None, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::MemoryLimit {
                kind: MemoryLimitKind::SpillShare,
                ..
            }
        ));
        assert_eq!(tight_res.size(), tight_before);
    }

    fn arb_data_type(max_depth: usize) -> BoxedStrategy<DataType> {
        let leaf = prop_oneof![
            Just(DataType::Null),
            Just(DataType::Bool),
            Just(DataType::I64),
            Just(DataType::F64),
            Just(DataType::cost_decimal()),
            Just(DataType::timestamp_utc()),
            Just(DataType::Date),
            Just(DataType::Interval),
            Just(DataType::Utf8),
            Just(DataType::Binary),
            Just(DataType::FixedBytes8),
            Just(DataType::FixedBytes16),
            Just(DataType::MapUtf8),
            Just(DataType::Events),
            Just(DataType::Links),
        ];
        if max_depth == 0 {
            leaf.boxed()
        } else {
            leaf.prop_recursive(max_depth as u32, 8, 2, |inner| {
                inner.prop_map(|dt| DataType::List(Box::new(dt)))
            })
            .boxed()
        }
    }

    fn arb_scalar_for_type(dt: &DataType, nullable: bool) -> BoxedStrategy<ScalarValue> {
        let non_null: BoxedStrategy<ScalarValue> = match dt {
            DataType::Null => Just(ScalarValue::Null(DataType::Null)).boxed(),
            DataType::Bool => any::<bool>().prop_map(ScalarValue::Bool).boxed(),
            DataType::I64 => any::<i64>().prop_map(ScalarValue::I64).boxed(),
            DataType::F64 => prop_oneof![any::<f64>(), Just(f64::NAN), Just(0.0), Just(-0.0),]
                .prop_map(ScalarValue::F64)
                .boxed(),
            DataType::Decimal128 { precision, scale } => {
                let p = *precision;
                let s = *scale;
                (-1_000_000i128..=1_000_000i128)
                    .prop_map(move |value| ScalarValue::Decimal128 {
                        value,
                        precision: p,
                        scale: s,
                    })
                    .boxed()
            }
            DataType::TimestampNs { utc } => {
                let u = *utc;
                any::<i64>()
                    .prop_map(move |value| ScalarValue::TimestampNs { value, utc: u })
                    .boxed()
            }
            DataType::Date => any::<i32>().prop_map(ScalarValue::Date).boxed(),
            DataType::Interval => (any::<i32>(), any::<i32>(), any::<i64>())
                .prop_map(|(months, days, nanos)| {
                    ScalarValue::Interval(Interval {
                        months,
                        days,
                        nanos,
                    })
                })
                .boxed(),
            DataType::Utf8 => prop::string::string_regex("[a-zA-Z0-9_é🚀]{0,8}")
                .unwrap()
                .prop_map(ScalarValue::Utf8)
                .boxed(),
            DataType::Binary => prop::collection::vec(any::<u8>(), 0..=8)
                .prop_map(ScalarValue::Binary)
                .boxed(),
            DataType::FixedBytes8 => any::<[u8; 8]>().prop_map(ScalarValue::FixedBytes8).boxed(),
            DataType::FixedBytes16 => any::<[u8; 16]>()
                .prop_map(ScalarValue::FixedBytes16)
                .boxed(),
            DataType::List(inner) => {
                let inner_dt = (**inner).clone();
                let elem_strat = arb_scalar_for_type(&inner_dt, true);
                prop::collection::vec(elem_strat, 0..=4)
                    .prop_map(move |values| ScalarValue::List {
                        item_type: inner_dt.clone(),
                        values,
                    })
                    .boxed()
            }
            DataType::MapUtf8 => prop::collection::vec(
                (
                    prop::string::string_regex("[a-z]{1,4}").unwrap(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                ),
                0..=3,
            )
            .prop_map(ScalarValue::MapUtf8)
            .boxed(),
            DataType::Events => prop::collection::vec(
                (
                    any::<i64>(),
                    prop::string::string_regex("[a-z]{1,4}").unwrap(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                )
                    .prop_map(|(time, name, attributes)| EventItem {
                        time,
                        name,
                        attributes,
                    }),
                0..=2,
            )
            .prop_map(ScalarValue::Events)
            .boxed(),
            DataType::Links => prop::collection::vec(
                (
                    any::<[u8; 16]>(),
                    any::<[u8; 8]>(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                )
                    .prop_map(|(trace_id, span_id, attributes)| LinkItem {
                        trace_id,
                        span_id,
                        attributes,
                    }),
                0..=2,
            )
            .prop_map(ScalarValue::Links)
            .boxed(),
        };

        if nullable && !matches!(dt, DataType::Null) {
            let dt_c = dt.clone();
            prop_oneof![
                3 => non_null,
                1 => Just(ScalarValue::Null(dt_c)),
            ]
            .boxed()
        } else {
            non_null
        }
    }

    fn build_column_from_scalars(dt: &DataType, scalars: &[ScalarValue], encoding: u8) -> Column {
        let row_cols: Vec<FlatColumn> = scalars
            .iter()
            .map(|s| {
                Column::try_new_const(s.clone(), 1)
                    .unwrap()
                    .materialize(None)
                    .unwrap()
            })
            .collect();
        let flat = if scalars.is_empty() {
            FlatColumn::empty(dt)
        } else {
            let refs: Vec<&FlatColumn> = row_cols.iter().collect();
            FlatColumn::concat(dt, &refs, scalars.len()).unwrap()
        };

        match encoding % 4 {
            0 => Column::Flat(flat),
            1 => {
                if scalars.is_empty() {
                    Column::try_new_const(ScalarValue::Null(dt.clone()), 0).unwrap()
                } else {
                    Column::try_new_const(scalars[0].clone(), scalars.len()).unwrap()
                }
            }
            2 => {
                if scalars.is_empty() {
                    Column::Dict(DictColumn::try_new(FlatColumn::empty(dt), vec![], None).unwrap())
                } else {
                    let codes: Vec<u32> = (0..scalars.len() as u32).collect();
                    Column::Dict(DictColumn::try_new(flat, codes, None).unwrap())
                }
            }
            _ => {
                if scalars.is_empty() {
                    Column::Rle(RleColumn::try_new(FlatColumn::empty(dt), vec![]).unwrap())
                } else {
                    let run_ends: Vec<u32> = (1..=scalars.len() as u32).collect();
                    Column::Rle(RleColumn::try_new(flat, run_ends).unwrap())
                }
            }
        }
    }

    fn arb_batch() -> BoxedStrategy<Batch> {
        (
            0usize..=32,
            prop::collection::vec((arb_data_type(3), any::<bool>(), 0u8..4u8), 0..=6),
        )
            .prop_flat_map(|(raw_len, col_specs)| {
                let col_strats: Vec<BoxedStrategy<(Field, Column)>> = col_specs
                    .into_iter()
                    .enumerate()
                    .map(|(idx, (dt, mut nullable, enc))| {
                        if matches!(dt, DataType::Null) {
                            nullable = true;
                        }
                        let name = format!("c{idx}");
                        let field = Field::new(name, dt.clone(), nullable);
                        let row_strat = prop::collection::vec(
                            arb_scalar_for_type(&dt, nullable),
                            raw_len..=raw_len,
                        );
                        row_strat
                            .prop_map(move |scalars| {
                                let col = build_column_from_scalars(&dt, &scalars, enc);
                                (field.clone(), col)
                            })
                            .boxed()
                    })
                    .collect();

                let sel_strat = if raw_len == 0 {
                    prop_oneof![
                        Just(Selection::All),
                        Just(Selection::Mask(Bitmap::new_zeroed(0))),
                        Just(Selection::Indices(vec![])),
                    ]
                    .boxed()
                } else {
                    prop_oneof![
                        Just(Selection::All),
                        prop::collection::vec(any::<bool>(), raw_len..=raw_len)
                            .prop_map(|b| Selection::Mask(Bitmap::from_bools(&b))),
                        prop::collection::vec(0..raw_len as u32, 0..=raw_len)
                            .prop_map(Selection::Indices),
                    ]
                    .boxed()
                };

                (col_strats, sel_strat, Just(raw_len)).prop_map(|(pairs, sel, raw_len)| {
                    let mut fields = Vec::with_capacity(pairs.len());
                    let mut cols = Vec::with_capacity(pairs.len());
                    for (mut f, c) in pairs {
                        if c.has_nulls() {
                            f.nullable = true;
                        }
                        fields.push(f);
                        cols.push(c);
                    }
                    let schema = Schema::try_new_arc(fields).unwrap();
                    Batch::try_new(schema, cols, raw_len, sel).unwrap()
                })
            })
            .boxed()
    }

    proptest! {
        #[test]
        fn proptest_round_trip_and_arbitrary_bytes_never_panic(
            batch in arb_batch(),
            garbage in prop::collection::vec(any::<u8>(), 0..=256),
        ) {
            let mut framed = Vec::new();
            encode_batch(&batch, &mut framed).unwrap();

            let scope = GlobalPool::standalone(64 * 1024 * 1024);
            let mut res = scope.register("prop", true);
            let decoded = decode_batch(&framed, Some(batch.schema()), Some(&mut res)).unwrap();
            prop_assert_eq!(res.size(), decoded.memory_bytes());
            prop_assert!(batch.semantic_eq(&decoded));

            // Arbitrary bytes never panic in decode_next or decode_batch
            let _ = decode_next(&garbage, 0);
            if !garbage.is_empty() {
                let _ = decode_next(&garbage, garbage.len() / 2);
            }
            let mut err_res = scope.register("prop_err", true);
            let _ = decode_batch(&garbage, None, Some(&mut err_res));
            // Pool size must be exact (0 if Err, memory_bytes if somehow valid)
            if decode_batch(&garbage, None, None).is_err() {
                prop_assert_eq!(err_res.size(), 0);
            }
        }
    }
}
