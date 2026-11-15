//! The narrow layer: one column stored as chunks, each with one of the S3 encodings and
//! described by a 56-byte block-table entry.
//!
//! M1.2 covers integers, dates, timestamps, floats, booleans and FixedSizeBinary.
//! [`encode_chunk`] and [`decode_chunk`] are inverses of each other.

mod bitpack;
mod chunker;
mod decode;
mod encode;

pub use chunker::chunk_ranges;
pub use decode::decode_chunk;
pub use encode::{EncodedChunk, encode_chunk};

use arrow::datatypes::DataType;

use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};

/// The most rows a chunk may hold. The `FSB_RLE` run lengths are `u16`.
pub const MAX_CHUNK_ROWS: u32 = 65_535;

/// The S3 encodings.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Every value equal and non-null. The value is in the entry; there is no body.
    Const = 0,
    /// Every value null. There is no body.
    ConstNull = 1,
    /// Frame of reference plus bit packing, for integers.
    ForBitpack = 2,
    /// Fixed-width little-endian values.
    Plain = 3,
    /// Fixed-size binary values, one after another.
    FsbRaw = 4,
    /// Fixed-size binary values as run-length encoded runs.
    FsbRle = 5,
    /// Dictionary codes, for Utf8 (M1.3).
    Dict = 6,
    /// Variable-length values (M1.4).
    Varbin = 7,
    /// Dictionary codes of list elements (M1.3).
    ListDict = 8,
    /// A bit per row, for booleans.
    Bool = 9,
    /// An Arrow IPC stream (M1.4).
    Ipc = 10,
}

impl Encoding {
    /// The encoding for a stored byte, or `None` if the value is unknown.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Const),
            1 => Some(Self::ConstNull),
            2 => Some(Self::ForBitpack),
            3 => Some(Self::Plain),
            4 => Some(Self::FsbRaw),
            5 => Some(Self::FsbRle),
            6 => Some(Self::Dict),
            7 => Some(Self::Varbin),
            8 => Some(Self::ListDict),
            9 => Some(Self::Bool),
            10 => Some(Self::Ipc),
            _ => None,
        }
    }
}

/// Block-entry flag bits.
pub mod flags {
    /// The body starts with a validity bitmap.
    pub const HAS_NULLS: u8 = 1;
    /// The entry's `min` and `max` hold a zone.
    pub const HAS_ZONE: u8 = 2;
    /// The chunk holds at least one NaN.
    pub const HAS_NAN: u8 = 4;
    /// Every row is null.
    pub const ALL_NULL: u8 = 8;
}

/// One block-table entry (S3, 56 bytes): where a chunk is and how to decode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEntry {
    /// First row of the chunk, within the column.
    pub first_row: u32,
    /// Rows in the chunk.
    pub n_rows: u32,
    /// Absolute offset of the body segment, 0 when there is none.
    pub segment_offset: u64,
    /// Length of the body segment, 0 when there is none.
    pub segment_len: u32,
    /// How the chunk is encoded.
    pub encoding: Encoding,
    /// [`flags`] bits.
    pub flags: u8,
    /// Length of the constant value in `min`, for `Const`.
    pub const_len: u16,
    /// Zone minimum, or the constant value.
    pub min: [u8; 16],
    /// Zone maximum.
    pub max: [u8; 16],
}

impl BlockEntry {
    /// Writes exactly 56 bytes.
    pub fn encode(&self, out: &mut Out) {
        out.u32(self.first_row);
        out.u32(self.n_rows);
        out.u64(self.segment_offset);
        out.u32(self.segment_len);
        out.u8(self.encoding as u8);
        out.u8(self.flags);
        out.u16(self.const_len);
        out.bytes(&self.min);
        out.bytes(&self.max);
    }

    /// Decodes one entry and checks that its fields agree with each other.
    pub fn decode(c: &mut Cursor<'_>) -> Result<Self> {
        let at = c.position();
        let first_row = c.u32("block entry")?;
        let n_rows = c.u32("block entry")?;
        let segment_offset = c.u64("block entry")?;
        let segment_len = c.u32("block entry")?;
        let encoding_at = c.position();
        let encoding = Encoding::from_u8(c.u8("block entry")?).ok_or(StrataError::Corrupt {
            what: "block encoding",
            offset: encoding_at,
        })?;
        let flags = c.u8("block entry")?;
        let const_len = c.u16("block entry")?;
        let min: [u8; 16] =
            c.bytes(16, "block entry")?
                .try_into()
                .map_err(|_| StrataError::Corrupt {
                    what: "block entry",
                    offset: at,
                })?;
        let max: [u8; 16] =
            c.bytes(16, "block entry")?
                .try_into()
                .map_err(|_| StrataError::Corrupt {
                    what: "block entry",
                    offset: at,
                })?;

        let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
        if const_len > 16 {
            return Err(corrupt("block const length"));
        }
        if (segment_offset == 0) != (segment_len == 0) {
            return Err(corrupt("block segment"));
        }
        let bodyless = matches!(encoding, Encoding::Const | Encoding::ConstNull);
        if bodyless != (segment_len == 0) {
            return Err(corrupt("block segment"));
        }
        Ok(Self {
            first_row,
            n_rows,
            segment_offset,
            segment_len,
            encoding,
            flags,
            const_len,
            min,
            max,
        })
    }
}

/// How a numeric type is stored. The class decides which encodings apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Integers, dates and timestamps: frame of reference over `i64`, zoned.
    Int,
    /// `UInt64`: plain values, no zone.
    Wide,
    /// Floats: plain bits, zoned over the non-NaN values.
    Float,
}

/// The physical shape of a numeric type.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Numeric {
    pub(crate) class: Class,
    /// Bytes per value.
    pub(crate) width: usize,
    /// Whether the value is sign-extended when read.
    pub(crate) signed: bool,
}

/// The numeric shape of `dt`, or `None` when it is not a numeric type of M1.2.
pub(crate) fn numeric_of(dt: &DataType) -> Option<Numeric> {
    let (class, width, signed) = match dt {
        DataType::Int8 => (Class::Int, 1, true),
        DataType::Int16 => (Class::Int, 2, true),
        DataType::Int32 => (Class::Int, 4, true),
        DataType::Int64 => (Class::Int, 8, true),
        DataType::UInt8 => (Class::Int, 1, false),
        DataType::UInt16 => (Class::Int, 2, false),
        DataType::UInt32 => (Class::Int, 4, false),
        DataType::UInt64 => (Class::Wide, 8, false),
        DataType::Float32 => (Class::Float, 4, false),
        DataType::Float64 => (Class::Float, 8, false),
        DataType::Date32 => (Class::Int, 4, true),
        DataType::Date64 => (Class::Int, 8, true),
        DataType::Timestamp(_, _) => (Class::Int, 8, true),
        _ => return None,
    };
    Some(Numeric {
        class,
        width,
        signed,
    })
}

/// The error for a type the writer cannot store yet. It names the type, never values.
pub(crate) fn unsupported(dt: &DataType) -> StrataError {
    StrataError::Unsupported {
        what: dt.to_string(),
    }
}

/// Sign-extends the low `width` bytes of `v` to 64 bits.
pub(crate) fn sign_extend(v: u64, width: usize) -> u64 {
    match width {
        0 => 0,
        1..=7 => {
            let shift = 64 - 8 * width as u32;
            (((v << shift) as i64) >> shift) as u64
        }
        _ => v,
    }
}

/// Whether `v` is the sign- (or zero-) extension of a value that fits in `width` bytes.
pub(crate) fn fits(v: u64, width: usize, signed: bool) -> bool {
    match width {
        0..=7 if signed => sign_extend(v, width) == v,
        0..=7 => v >> (8 * width as u32) == 0,
        _ => true,
    }
}
