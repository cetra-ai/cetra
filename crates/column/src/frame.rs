//! Versioned, CRC-32C-checksummed, LZ4-compressed, DoS-bounded binary batch framing.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Range;

use lz4_flex::frame::{FrameDecoder, FrameEncoder};

use crate::column::{reconcile_reservation, rollback_reservation};
use crate::{
    Batch, BinaryData, Bitmap, Column, ColumnError, DataType, DictColumn, EventItem, EventsData,
    Field, FlatColumn, FlatData, Interval, LinkItem, LinksData, MAX_NESTING_DEPTH, MapData,
    Reservation, RleColumn, ScalarValue, Schema, Utf8Data,
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

fn validate_header_fields(hdr: &[u8; FRAME_HEADER_LEN]) -> Result<ParsedHeader, &'static str> {
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

struct ReservedVecWriter<'a> {
    buf: Vec<u8>,
    pool: Option<&'a mut Reservation>,
    err: Option<ColumnError>,
}

impl Write for ReservedVecWriter<'_> {
    fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
        if let Some(res) = self.pool.as_deref_mut()
            && let Err(e) = res.try_grow(src.len())
        {
            self.err = Some(e);
            return Err(std::io::Error::other("reservation memory limit exceeded"));
        }
        self.buf.extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn data_type_wire_len(dt: &DataType) -> usize {
    match dt {
        DataType::Decimal128 { .. } => 3,
        DataType::TimestampNs { .. } => 2,
        DataType::List(inner) => 1 + data_type_wire_len(inner),
        _ => 1,
    }
}

fn optional_bitmap_wire_len(bm: Option<&Bitmap>) -> usize {
    bm.map_or(1, |b| 1 + b.len().div_ceil(8))
}

fn utf8_wire_len(u: &Utf8Data) -> Result<usize, ColumnError> {
    u.offsets()
        .len()
        .checked_mul(4)
        .and_then(|s| s.checked_add(4))
        .and_then(|s| s.checked_add(u.bytes().len()))
        .ok_or(ColumnError::Overflow {
            reason: "utf8 wire length overflow",
        })
}

fn binary_wire_len(b: &BinaryData) -> Result<usize, ColumnError> {
    b.offsets()
        .len()
        .checked_mul(4)
        .and_then(|s| s.checked_add(4))
        .and_then(|s| s.checked_add(b.bytes().len()))
        .ok_or(ColumnError::Overflow {
            reason: "binary wire length overflow",
        })
}

fn flat_data_wire_len(data: &FlatData, max_decoded: u32) -> Result<usize, ColumnError> {
    match data {
        FlatData::Null(_) => Ok(0),
        FlatData::Bool(bm) => Ok(bm.len().div_ceil(8)),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            v.len().checked_mul(8).ok_or(ColumnError::Overflow {
                reason: "wire length overflow",
            })
        }
        FlatData::F64(v) => v.len().checked_mul(8).ok_or(ColumnError::Overflow {
            reason: "wire length overflow",
        }),
        FlatData::FixedBytes8(v) => v.len().checked_mul(8).ok_or(ColumnError::Overflow {
            reason: "wire length overflow",
        }),
        FlatData::Decimal128 { values, .. } => {
            values.len().checked_mul(16).ok_or(ColumnError::Overflow {
                reason: "wire length overflow",
            })
        }
        FlatData::Interval(values) => values.len().checked_mul(16).ok_or(ColumnError::Overflow {
            reason: "wire length overflow",
        }),
        FlatData::FixedBytes16(values) => {
            values.len().checked_mul(16).ok_or(ColumnError::Overflow {
                reason: "wire length overflow",
            })
        }
        FlatData::Date(v) => v.len().checked_mul(4).ok_or(ColumnError::Overflow {
            reason: "wire length overflow",
        }),
        FlatData::Utf8(u) => utf8_wire_len(u),
        FlatData::Binary(b) => binary_wire_len(b),
        FlatData::List {
            offsets, values, ..
        } => {
            let off_bytes = offsets.len().checked_mul(4).ok_or(ColumnError::Overflow {
                reason: "list wire length overflow",
            })?;
            let child_bytes = column_wire_len(values, max_decoded)?;
            off_bytes
                .checked_add(4)
                .and_then(|s| s.checked_add(child_bytes))
                .ok_or(ColumnError::Overflow {
                    reason: "list wire length overflow",
                })
        }
        FlatData::MapUtf8(m) => {
            let off_bytes = m
                .offsets()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "map wire length overflow",
                })?;
            let k_bytes = utf8_wire_len(m.keys())?;
            let v_bytes = utf8_wire_len(m.values())?;
            off_bytes
                .checked_add(4)
                .and_then(|s| s.checked_add(k_bytes))
                .and_then(|s| s.checked_add(v_bytes))
                .and_then(|s| s.checked_add(optional_bitmap_wire_len(m.value_nulls())))
                .ok_or(ColumnError::Overflow {
                    reason: "map wire length overflow",
                })
        }
        FlatData::Events(e) => {
            let off_bytes = e
                .offsets()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "events wire length overflow",
                })?;
            let times_bytes = e
                .times()
                .len()
                .checked_mul(8)
                .ok_or(ColumnError::Overflow {
                    reason: "events wire length overflow",
                })?;
            let n_bytes = utf8_wire_len(e.names())?;
            let a_bytes = utf8_wire_len(e.attributes())?;
            off_bytes
                .checked_add(4)
                .and_then(|s| s.checked_add(times_bytes))
                .and_then(|s| s.checked_add(n_bytes))
                .and_then(|s| s.checked_add(a_bytes))
                .and_then(|s| s.checked_add(optional_bitmap_wire_len(e.attribute_nulls())))
                .ok_or(ColumnError::Overflow {
                    reason: "events wire length overflow",
                })
        }
        FlatData::Links(l) => {
            let off_bytes = l
                .offsets()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })?;
            let tid_bytes = l
                .trace_ids()
                .len()
                .checked_mul(16)
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })?;
            let sid_bytes = l
                .span_ids()
                .len()
                .checked_mul(8)
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })?;
            let a_bytes = utf8_wire_len(l.attributes())?;
            off_bytes
                .checked_add(4)
                .and_then(|s| s.checked_add(tid_bytes))
                .and_then(|s| s.checked_add(sid_bytes))
                .and_then(|s| s.checked_add(a_bytes))
                .and_then(|s| s.checked_add(optional_bitmap_wire_len(l.attribute_nulls())))
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })
        }
    }
}

fn flat_column_wire_len(fc: &FlatColumn, max_decoded: u32) -> Result<usize, ColumnError> {
    optional_bitmap_wire_len(fc.nulls())
        .checked_add(flat_data_wire_len(fc.data(), max_decoded)?)
        .ok_or(ColumnError::Overflow {
            reason: "flat column wire length overflow",
        })
}

fn scalar_groups_flat_data_wire_len(
    dt: &DataType,
    groups: &[&[ScalarValue]],
    max_decoded: u32,
) -> Result<usize, ColumnError> {
    let mut count = 0usize;
    for g in groups {
        count = count.checked_add(g.len()).ok_or(ColumnError::Overflow {
            reason: "const scalar length overflow",
        })?;
    }
    let len = match dt {
        DataType::Null => Ok(0),
        DataType::Bool => Ok(count.div_ceil(8)),
        DataType::I64 | DataType::TimestampNs { .. } | DataType::F64 | DataType::FixedBytes8 => {
            count.checked_mul(8).ok_or(ColumnError::Overflow {
                reason: "const column wire length overflow",
            })
        }
        DataType::Decimal128 { .. } | DataType::Interval | DataType::FixedBytes16 => {
            count.checked_mul(16).ok_or(ColumnError::Overflow {
                reason: "const column wire length overflow",
            })
        }
        DataType::Date => count.checked_mul(4).ok_or(ColumnError::Overflow {
            reason: "const column wire length overflow",
        }),
        DataType::Utf8 => {
            let mut str_bytes = 0usize;
            for g in groups {
                for v in *g {
                    if let ScalarValue::Utf8(s) = v {
                        str_bytes =
                            str_bytes
                                .checked_add(s.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "utf8 wire length overflow",
                                })?;
                    }
                }
            }
            let _ = checked_u32(str_bytes, "utf8 bytes length exceeds u32::MAX")?;
            count
                .checked_add(1)
                .and_then(|s| s.checked_mul(4))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(str_bytes))
                .ok_or(ColumnError::Overflow {
                    reason: "utf8 wire length overflow",
                })
        }
        DataType::Binary => {
            let mut bin_bytes = 0usize;
            for g in groups {
                for v in *g {
                    if let ScalarValue::Binary(b) = v {
                        bin_bytes =
                            bin_bytes
                                .checked_add(b.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "binary wire length overflow",
                                })?;
                    }
                }
            }
            let _ = checked_u32(bin_bytes, "binary bytes length exceeds u32::MAX")?;
            count
                .checked_add(1)
                .and_then(|s| s.checked_mul(4))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(bin_bytes))
                .ok_or(ColumnError::Overflow {
                    reason: "binary wire length overflow",
                })
        }
        DataType::List(inner) => {
            let off_bytes = count.checked_add(1).and_then(|s| s.checked_mul(4)).ok_or(
                ColumnError::Overflow {
                    reason: "list wire length overflow",
                },
            )?;
            let mut child_groups: Vec<&[ScalarValue]> = Vec::new();
            let mut child_len = 0usize;
            let mut has_nulls = false;
            let check_nulls = !matches!(inner.as_ref(), DataType::Null);
            for g in groups {
                for v in *g {
                    if let ScalarValue::List { values, .. } = v {
                        child_len =
                            child_len
                                .checked_add(values.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "list scalar length exceeds u32::MAX",
                                })?;
                        if check_nulls && !has_nulls && values.iter().any(ScalarValue::is_null) {
                            has_nulls = true;
                        }
                        child_groups.push(values.as_slice());
                    }
                }
            }
            let _ = checked_u32(child_len, "list scalar length exceeds u32::MAX")?;
            let null_wire = if has_nulls {
                1usize
                    .checked_add(child_len.div_ceil(8))
                    .ok_or(ColumnError::Overflow {
                        reason: "list wire length overflow",
                    })?
            } else {
                1
            };
            let child_data_wire =
                scalar_groups_flat_data_wire_len(inner, &child_groups, max_decoded)?;
            off_bytes
                .checked_add(4)
                .and_then(|s| s.checked_add(1))
                .and_then(|s| s.checked_add(null_wire))
                .and_then(|s| s.checked_add(child_data_wire))
                .ok_or(ColumnError::Overflow {
                    reason: "list wire length overflow",
                })
        }
        DataType::MapUtf8 => {
            let row_off = count.checked_add(1).and_then(|s| s.checked_mul(4)).ok_or(
                ColumnError::Overflow {
                    reason: "map wire length overflow",
                },
            )?;
            let mut entry_count = 0usize;
            let mut k_bytes = 0usize;
            let mut v_bytes = 0usize;
            let mut has_nulls = false;
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        entry_count = entry_count.checked_add(entries.len()).ok_or(
                            ColumnError::Overflow {
                                reason: "map scalar entries exceed u32::MAX",
                            },
                        )?;
                        for (k, mv) in entries {
                            k_bytes =
                                k_bytes.checked_add(k.len()).ok_or(ColumnError::Overflow {
                                    reason: "map wire length overflow",
                                })?;
                            if let Some(val) = mv {
                                v_bytes = v_bytes.checked_add(val.len()).ok_or(
                                    ColumnError::Overflow {
                                        reason: "map wire length overflow",
                                    },
                                )?;
                            } else {
                                has_nulls = true;
                            }
                        }
                    }
                }
            }
            let _ = checked_u32(entry_count, "map entry count exceeds u32::MAX")?;
            let _ = checked_u32(k_bytes, "utf8 bytes length exceeds u32::MAX")?;
            let _ = checked_u32(v_bytes, "utf8 bytes length exceeds u32::MAX")?;
            let entry_off = entry_count
                .checked_add(1)
                .and_then(|s| s.checked_mul(4))
                .ok_or(ColumnError::Overflow {
                    reason: "map wire length overflow",
                })?;
            let null_wire = if has_nulls {
                1usize
                    .checked_add(entry_count.div_ceil(8))
                    .ok_or(ColumnError::Overflow {
                        reason: "map wire length overflow",
                    })?
            } else {
                1
            };
            row_off
                .checked_add(4)
                .and_then(|s| s.checked_add(entry_off))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(k_bytes))
                .and_then(|s| s.checked_add(entry_off))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(v_bytes))
                .and_then(|s| s.checked_add(null_wire))
                .ok_or(ColumnError::Overflow {
                    reason: "map wire length overflow",
                })
        }
        DataType::Events => {
            let row_off = count.checked_add(1).and_then(|s| s.checked_mul(4)).ok_or(
                ColumnError::Overflow {
                    reason: "events wire length overflow",
                },
            )?;
            let mut ev_count = 0usize;
            let mut name_bytes = 0usize;
            let mut attr_bytes = 0usize;
            let mut has_nulls = false;
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        ev_count =
                            ev_count
                                .checked_add(evs.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "events scalar count exceeds u32::MAX",
                                })?;
                        for ev in evs {
                            name_bytes = name_bytes.checked_add(ev.name.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "events wire length overflow",
                                },
                            )?;
                            if let Some(attr) = &ev.attributes {
                                attr_bytes = attr_bytes.checked_add(attr.len()).ok_or(
                                    ColumnError::Overflow {
                                        reason: "events wire length overflow",
                                    },
                                )?;
                            } else {
                                has_nulls = true;
                            }
                        }
                    }
                }
            }
            let _ = checked_u32(ev_count, "events count exceeds u32::MAX")?;
            let _ = checked_u32(name_bytes, "utf8 bytes length exceeds u32::MAX")?;
            let _ = checked_u32(attr_bytes, "utf8 bytes length exceeds u32::MAX")?;
            let times_bytes = ev_count.checked_mul(8).ok_or(ColumnError::Overflow {
                reason: "events wire length overflow",
            })?;
            let entry_off = ev_count
                .checked_add(1)
                .and_then(|s| s.checked_mul(4))
                .ok_or(ColumnError::Overflow {
                    reason: "events wire length overflow",
                })?;
            let null_wire = if has_nulls {
                1usize
                    .checked_add(ev_count.div_ceil(8))
                    .ok_or(ColumnError::Overflow {
                        reason: "events wire length overflow",
                    })?
            } else {
                1
            };
            row_off
                .checked_add(4)
                .and_then(|s| s.checked_add(times_bytes))
                .and_then(|s| s.checked_add(entry_off))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(name_bytes))
                .and_then(|s| s.checked_add(entry_off))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(attr_bytes))
                .and_then(|s| s.checked_add(null_wire))
                .ok_or(ColumnError::Overflow {
                    reason: "events wire length overflow",
                })
        }
        DataType::Links => {
            let row_off = count.checked_add(1).and_then(|s| s.checked_mul(4)).ok_or(
                ColumnError::Overflow {
                    reason: "links wire length overflow",
                },
            )?;
            let mut lk_count = 0usize;
            let mut attr_bytes = 0usize;
            let mut has_nulls = false;
            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        lk_count =
                            lk_count
                                .checked_add(lks.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "links scalar count exceeds u32::MAX",
                                })?;
                        for lk in lks {
                            if let Some(attr) = &lk.attributes {
                                attr_bytes = attr_bytes.checked_add(attr.len()).ok_or(
                                    ColumnError::Overflow {
                                        reason: "links wire length overflow",
                                    },
                                )?;
                            } else {
                                has_nulls = true;
                            }
                        }
                    }
                }
            }
            let _ = checked_u32(lk_count, "links count exceeds u32::MAX")?;
            let _ = checked_u32(attr_bytes, "utf8 bytes length exceeds u32::MAX")?;
            let ids_bytes = lk_count.checked_mul(24).ok_or(ColumnError::Overflow {
                reason: "links wire length overflow",
            })?;
            let entry_off = lk_count
                .checked_add(1)
                .and_then(|s| s.checked_mul(4))
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })?;
            let null_wire = if has_nulls {
                1usize
                    .checked_add(lk_count.div_ceil(8))
                    .ok_or(ColumnError::Overflow {
                        reason: "links wire length overflow",
                    })?
            } else {
                1
            };
            row_off
                .checked_add(4)
                .and_then(|s| s.checked_add(ids_bytes))
                .and_then(|s| s.checked_add(entry_off))
                .and_then(|s| s.checked_add(4))
                .and_then(|s| s.checked_add(attr_bytes))
                .and_then(|s| s.checked_add(null_wire))
                .ok_or(ColumnError::Overflow {
                    reason: "links wire length overflow",
                })
        }
    }?;
    if len > max_decoded as usize {
        return Err(ColumnError::InvalidFrame {
            reason: "uncompressed body exceeds maximum decoded bytes",
        });
    }
    Ok(len)
}

fn column_wire_len(col: &Column, max_decoded: u32) -> Result<usize, ColumnError> {
    match col {
        Column::Flat(fc) => 1usize
            .checked_add(flat_column_wire_len(fc, max_decoded)?)
            .ok_or(ColumnError::Overflow {
                reason: "column wire length overflow",
            }),
        Column::Const { value, .. } => {
            if value.is_null() {
                Ok(2)
            } else {
                let dt = value.data_type();
                let data_wire = scalar_groups_flat_data_wire_len(
                    &dt,
                    &[std::slice::from_ref(value)],
                    max_decoded,
                )?;
                2usize.checked_add(data_wire).ok_or(ColumnError::Overflow {
                    reason: "const column wire length overflow",
                })
            }
        }
        Column::Dict(d) => {
            let val_bytes = flat_column_wire_len(d.values(), max_decoded)?;
            let code_bytes = d
                .codes()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "dict codes wire length overflow",
                })?;
            5usize
                .checked_add(val_bytes)
                .and_then(|s| s.checked_add(code_bytes))
                .and_then(|s| s.checked_add(optional_bitmap_wire_len(d.nulls())))
                .ok_or(ColumnError::Overflow {
                    reason: "dict column wire length overflow",
                })
        }
        Column::Rle(r) => {
            let val_bytes = flat_column_wire_len(r.values(), max_decoded)?;
            let end_bytes = r
                .run_ends()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "rle run_ends wire length overflow",
                })?;
            5usize
                .checked_add(val_bytes)
                .and_then(|s| s.checked_add(end_bytes))
                .ok_or(ColumnError::Overflow {
                    reason: "rle column wire length overflow",
                })
        }
    }
}

fn estimate_body_len(batch: &Batch, max_decoded: u32) -> Result<usize, ColumnError> {
    let mut total = 8usize;
    for field in batch.schema().fields() {
        total = total
            .checked_add(4)
            .and_then(|s| s.checked_add(field.name.len()))
            .and_then(|s| s.checked_add(1))
            .and_then(|s| s.checked_add(data_type_wire_len(&field.data_type)))
            .ok_or(ColumnError::Overflow {
                reason: "schema wire length overflow",
            })?;
    }
    for col in batch.columns() {
        total = total
            .checked_add(column_wire_len(col, max_decoded)?)
            .ok_or(ColumnError::Overflow {
                reason: "batch body wire length overflow",
            })?;
    }
    Ok(total)
}

/// Encodes `batch` (compacting active rows first if `!selection.is_all(raw_len)`, preserving column encodings
/// when all rows are active) and appends one frame to `out`.
///
/// Postcondition: reserves peak temporary scratch (`compacted`, `body`, `compressed`) and `out` growth inside
/// `pool` while encoding (dropping and shrinking intermediate buffers as soon as no longer needed), and
/// restores `pool` to its pre-call `size()` on both `Ok` and `Err`. On error, `out` is left unchanged.
pub fn encode_batch(
    batch: &Batch,
    out: &mut Vec<u8>,
    pool: Option<&mut Reservation>,
) -> Result<(), ColumnError> {
    encode_batch_with_limit(batch, out, pool, MAX_DECODED_BYTES)
}

/// [`encode_batch`] with a custom decoded-byte cap (for testing small caps).
///
/// Postcondition: reserves peak temporary scratch and `out` growth inside `pool` during encoding and restores
/// `pool` to its pre-call `size()` on both `Ok` and `Err`. On error, `out` is left unchanged.
pub fn encode_batch_with_limit(
    batch: &Batch,
    out: &mut Vec<u8>,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
) -> Result<(), ColumnError> {
    let before = pool.as_ref().map_or(0, |p| p.size());
    let res = encode_batch_inner(batch, out, pool.as_deref_mut(), max_decoded, before);
    rollback_reservation(pool, before);
    res
}

fn encode_batch_inner(
    batch: &Batch,
    out: &mut Vec<u8>,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
    before: usize,
) -> Result<(), ColumnError> {
    let effective_max = max_decoded.min(MAX_DECODED_BYTES);
    if batch.schema().len() > MAX_COLUMNS as usize {
        return Err(ColumnError::InvalidFrame {
            reason: "column count exceeds MAX_COLUMNS",
        });
    }
    let num_rows = batch.num_rows();
    if num_rows > u32::MAX as usize {
        return Err(ColumnError::Overflow {
            reason: "batch row count exceeds u32::MAX",
        });
    }
    if num_rows > effective_max as usize {
        return Err(ColumnError::InvalidFrame {
            reason: "batch raw_len exceeds max_decoded",
        });
    }

    let mut body = Vec::new();
    let body_len = {
        let compacted_owner;
        let target = if batch.selection().is_all(batch.raw_len()) {
            batch
        } else {
            compacted_owner = batch.compact(pool.as_deref_mut())?;
            &compacted_owner
        };
        let est = estimate_body_len(target, effective_max)?;
        if est > effective_max as usize {
            return Err(ColumnError::InvalidFrame {
                reason: "uncompressed body exceeds maximum decoded bytes",
            });
        }
        validate_batch_decode_limits(target, effective_max, pool.as_deref_mut())?;
        let compacted_bytes = pool.as_ref().map_or(0, |p| p.size().saturating_sub(before));
        let peak_body = compacted_bytes
            .checked_add(est)
            .ok_or(ColumnError::Overflow {
                reason: "encode scratch reservation overflow",
            })?;
        reconcile_reservation(pool.as_deref_mut(), before, peak_body)?;
        body.reserve_exact(est);
        write_body(target, &mut body)?;
        debug_assert_eq!(body.len(), est);
        body.len()
    };
    reconcile_reservation(pool.as_deref_mut(), before, body_len)?;

    let uncompressed_len = u32::try_from(body_len).map_err(|_| ColumnError::Overflow {
        reason: "uncompressed body length exceeds u32::MAX",
    })?;

    let mut compressed_owner: Option<Vec<u8>> = None;
    let flags: u8 = if body_len >= LZ4_MIN_COMPRESS_LEN {
        let writer = ReservedVecWriter {
            buf: Vec::new(),
            pool: pool.as_deref_mut(),
            err: None,
        };
        let mut encoder = FrameEncoder::new(writer);
        if encoder.write_all(&body).is_err() {
            let mut inner = encoder.into_inner();
            if let Some(col_err) = inner.err.take() {
                return Err(col_err);
            }
            return Err(ColumnError::InvalidFrame {
                reason: "lz4 compression failed",
            });
        }
        if encoder.try_finish().is_err() {
            let mut inner = encoder.into_inner();
            if let Some(col_err) = inner.err.take() {
                return Err(col_err);
            }
            return Err(ColumnError::InvalidFrame {
                reason: "lz4 compression finish failed",
            });
        }
        let mut writer = encoder.into_inner();
        if let Some(col_err) = writer.err.take() {
            return Err(col_err);
        }
        if writer.buf.len() < body_len {
            compressed_owner = Some(writer.buf);
            FLAG_LZ4
        } else {
            0
        }
    } else {
        0
    };

    if flags == FLAG_LZ4 {
        drop(body);
        body = Vec::new();
    }
    let payload: &[u8] = if let Some(ref comp) = compressed_owner {
        reconcile_reservation(pool.as_deref_mut(), before, comp.len())?;
        comp
    } else {
        reconcile_reservation(pool.as_deref_mut(), before, body_len)?;
        &body
    };

    let payload_len = u32::try_from(payload.len()).map_err(|_| ColumnError::Overflow {
        reason: "payload length exceeds u32::MAX",
    })?;
    if payload_len > MAX_PAYLOAD {
        return Err(ColumnError::InvalidFrame {
            reason: "encoded payload exceeds MAX_PAYLOAD",
        });
    }

    let frame_len = FRAME_HEADER_LEN + payload.len();
    if let Some(res) = pool {
        res.try_grow(frame_len)?;
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

    out.reserve(frame_len);
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    Ok(())
}

/// Scans the next frame starting at `offset` in `buf`, checking header invariants and CRC-32C without decompressing.
pub fn decode_next(buf: &[u8], offset: usize) -> FrameSlice {
    if offset == buf.len() {
        return FrameSlice::End;
    }
    let Some(hdr) = buf
        .get(offset..)
        .and_then(|rest| rest.first_chunk::<FRAME_HEADER_LEN>())
    else {
        return FrameSlice::Short { offset };
    };
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

struct EofTrackingSlice<'a> {
    slice: &'a [u8],
    hit_eof: bool,
}

impl Read for EofTrackingSlice<'_> {
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        if self.slice.is_empty() {
            self.hit_eof = true;
            return Ok(0);
        }
        let n = self.slice.len().min(dst.len());
        dst[..n].copy_from_slice(&self.slice[..n]);
        self.slice = &self.slice[n..];
        Ok(n)
    }
}

fn decode_batch_inner(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
) -> Result<Batch, ColumnError> {
    let Some(hdr) = frame.first_chunk::<FRAME_HEADER_LEN>() else {
        return Err(ColumnError::InvalidFrame {
            reason: "frame shorter than header",
        });
    };
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
        let mut cursor = EofTrackingSlice {
            slice: payload,
            hit_eof: false,
        };
        let mut decoder = FrameDecoder::new(&mut cursor).take(limit);
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .map_err(|_| ColumnError::InvalidFrame {
                reason: "lz4 decompression failed",
            })?;
        drop(decoder);
        if buf.len() != scratch_len {
            return Err(ColumnError::InvalidFrame {
                reason: "lz4 decoded length does not match header",
            });
        }
        if cursor.hit_eof {
            return Err(ColumnError::InvalidFrame {
                reason: "truncated lz4 frame endmark",
            });
        }
        if !cursor.slice.is_empty() {
            return Err(ColumnError::InvalidFrame {
                reason: "trailing bytes after lz4 frame",
            });
        }
        decoded_owner = buf;
        &decoded_owner
    } else {
        payload
    };

    let mut reader = BodyReader::new(body);
    let raw_len_u32 = reader.read_u32()?;
    if raw_len_u32 > max_decoded {
        return Err(ColumnError::InvalidFrame {
            reason: "batch raw_len exceeds max_decoded",
        });
    }
    let raw_len = raw_len_u32 as usize;
    let mut logical_elements = u64::from(raw_len_u32);
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

    let max_decoded_usize = max_decoded as usize;
    let mut materialized_bytes = 0usize;
    let mut columns = Vec::with_capacity(col_count);
    for field in schema.fields() {
        let col = read_column(
            &mut reader,
            &field.data_type,
            raw_len,
            0,
            pool.as_deref_mut(),
            max_decoded,
            &mut logical_elements,
        )?;
        let col_bytes = estimate_column_materialized_bytes(
            &col,
            max_decoded_usize - materialized_bytes,
            pool.as_deref_mut(),
        )?;
        materialized_bytes += col_bytes;
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

fn write_scalar_groups_flat_data(
    out: &mut Vec<u8>,
    dt: &DataType,
    groups: &[&[ScalarValue]],
) -> Result<(), ColumnError> {
    let count: usize = groups.iter().map(|g| g.len()).sum();
    match dt {
        DataType::Null => {}
        DataType::Bool => {
            let start = out.len();
            out.resize(start + count.div_ceil(8), 0);
            let mut i = 0usize;
            for g in groups {
                for v in *g {
                    if let ScalarValue::Bool(true) = v {
                        out[start + i / 8] |= 1u8 << (i % 8);
                    }
                    i += 1;
                }
            }
        }
        DataType::I64 | DataType::TimestampNs { .. } => {
            for g in groups {
                for v in *g {
                    let x = match v {
                        ScalarValue::I64(x) | ScalarValue::TimestampNs { value: x, .. } => *x,
                        _ => 0,
                    };
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        DataType::F64 => {
            for g in groups {
                for v in *g {
                    let x = match v {
                        ScalarValue::F64(x) => *x,
                        _ => 0.0,
                    };
                    out.extend_from_slice(&x.to_bits().to_le_bytes());
                }
            }
        }
        DataType::Decimal128 { .. } => {
            for g in groups {
                for v in *g {
                    let x = match v {
                        ScalarValue::Decimal128 { value, .. } => *value,
                        _ => 0,
                    };
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        DataType::Date => {
            for g in groups {
                for v in *g {
                    let x = match v {
                        ScalarValue::Date(x) => *x,
                        _ => 0,
                    };
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        DataType::Interval => {
            for g in groups {
                for v in *g {
                    let iv = match v {
                        ScalarValue::Interval(iv) => *iv,
                        _ => Interval::default(),
                    };
                    out.extend_from_slice(&iv.months.to_le_bytes());
                    out.extend_from_slice(&iv.days.to_le_bytes());
                    out.extend_from_slice(&iv.nanos.to_le_bytes());
                }
            }
        }
        DataType::FixedBytes8 => {
            for g in groups {
                for v in *g {
                    let b = match v {
                        ScalarValue::FixedBytes8(b) => *b,
                        _ => [0u8; 8],
                    };
                    out.extend_from_slice(&b);
                }
            }
        }
        DataType::FixedBytes16 => {
            for g in groups {
                for v in *g {
                    let b = match v {
                        ScalarValue::FixedBytes16(b) => *b,
                        _ => [0u8; 16],
                    };
                    out.extend_from_slice(&b);
                }
            }
        }
        DataType::Utf8 => {
            let mut off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Utf8(s) = v {
                        off += s.len();
                    }
                    let u = checked_u32(off, "utf8 bytes length exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let bytes_len = checked_u32(off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&bytes_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Utf8(s) = v {
                        out.extend_from_slice(s.as_bytes());
                    }
                }
            }
        }
        DataType::Binary => {
            let mut off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Binary(b) = v {
                        off += b.len();
                    }
                    let u = checked_u32(off, "binary bytes length exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let bytes_len = checked_u32(off, "binary bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&bytes_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Binary(b) = v {
                        out.extend_from_slice(b);
                    }
                }
            }
        }
        DataType::List(inner) => {
            let mut off = 0usize;
            let mut child_groups: Vec<&[ScalarValue]> = Vec::new();
            let mut has_nulls = false;
            let check_nulls = !matches!(inner.as_ref(), DataType::Null);
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::List { values, .. } = v {
                        off += values.len();
                        if check_nulls && !has_nulls && values.iter().any(ScalarValue::is_null) {
                            has_nulls = true;
                        }
                        child_groups.push(values.as_slice());
                    }
                    let u = checked_u32(off, "list child length exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let child_len = checked_u32(off, "list child length exceeds u32::MAX")?;
            out.extend_from_slice(&child_len.to_le_bytes());
            out.push(0); // Column::Flat
            if has_nulls {
                out.push(1);
                let bm_start = out.len();
                out.resize(bm_start + off.div_ceil(8), 0);
                let mut i = 0usize;
                for cg in &child_groups {
                    for cv in *cg {
                        if !cv.is_null() {
                            out[bm_start + i / 8] |= 1u8 << (i % 8);
                        }
                        i += 1;
                    }
                }
            } else {
                out.push(0);
            }
            write_scalar_groups_flat_data(out, inner, &child_groups)?;
        }
        DataType::MapUtf8 => {
            let mut entry_count = 0usize;
            let mut has_nulls = false;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        entry_count += entries.len();
                        if !has_nulls && entries.iter().any(|(_, mv)| mv.is_none()) {
                            has_nulls = true;
                        }
                    }
                    let u = checked_u32(entry_count, "map entry count exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let ec_u32 = checked_u32(entry_count, "map entry count exceeds u32::MAX")?;
            out.extend_from_slice(&ec_u32.to_le_bytes());

            // keys
            let mut k_off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        for (k, _) in entries {
                            k_off += k.len();
                            let u = checked_u32(k_off, "utf8 bytes length exceeds u32::MAX")?;
                            out.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                }
            }
            let k_len = checked_u32(k_off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&k_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        for (k, _) in entries {
                            out.extend_from_slice(k.as_bytes());
                        }
                    }
                }
            }

            // values
            let mut v_off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        for (_, mv) in entries {
                            if let Some(val) = mv {
                                v_off += val.len();
                            }
                            let u = checked_u32(v_off, "utf8 bytes length exceeds u32::MAX")?;
                            out.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                }
            }
            let v_len = checked_u32(v_off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&v_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::MapUtf8(entries) = v {
                        for (_, mv) in entries {
                            if let Some(val) = mv {
                                out.extend_from_slice(val.as_bytes());
                            }
                        }
                    }
                }
            }

            if has_nulls {
                out.push(1);
                let bm_start = out.len();
                out.resize(bm_start + entry_count.div_ceil(8), 0);
                let mut i = 0usize;
                for g in groups {
                    for v in *g {
                        if let ScalarValue::MapUtf8(entries) = v {
                            for (_, mv) in entries {
                                if mv.is_some() {
                                    out[bm_start + i / 8] |= 1u8 << (i % 8);
                                }
                                i += 1;
                            }
                        }
                    }
                }
            } else {
                out.push(0);
            }
        }
        DataType::Events => {
            let mut ev_count = 0usize;
            let mut has_nulls = false;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        ev_count += evs.len();
                        if !has_nulls && evs.iter().any(|ev| ev.attributes.is_none()) {
                            has_nulls = true;
                        }
                    }
                    let u = checked_u32(ev_count, "events count exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let ev_u32 = checked_u32(ev_count, "events count exceeds u32::MAX")?;
            out.extend_from_slice(&ev_u32.to_le_bytes());

            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        for ev in evs {
                            out.extend_from_slice(&ev.time.to_le_bytes());
                        }
                    }
                }
            }

            // names
            let mut n_off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        for ev in evs {
                            n_off += ev.name.len();
                            let u = checked_u32(n_off, "utf8 bytes length exceeds u32::MAX")?;
                            out.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                }
            }
            let n_len = checked_u32(n_off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&n_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        for ev in evs {
                            out.extend_from_slice(ev.name.as_bytes());
                        }
                    }
                }
            }

            // attributes
            let mut a_off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        for ev in evs {
                            if let Some(attr) = &ev.attributes {
                                a_off += attr.len();
                            }
                            let u = checked_u32(a_off, "utf8 bytes length exceeds u32::MAX")?;
                            out.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                }
            }
            let a_len = checked_u32(a_off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&a_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Events(evs) = v {
                        for ev in evs {
                            if let Some(attr) = &ev.attributes {
                                out.extend_from_slice(attr.as_bytes());
                            }
                        }
                    }
                }
            }

            if has_nulls {
                out.push(1);
                let bm_start = out.len();
                out.resize(bm_start + ev_count.div_ceil(8), 0);
                let mut i = 0usize;
                for g in groups {
                    for v in *g {
                        if let ScalarValue::Events(evs) = v {
                            for ev in evs {
                                if ev.attributes.is_some() {
                                    out[bm_start + i / 8] |= 1u8 << (i % 8);
                                }
                                i += 1;
                            }
                        }
                    }
                }
            } else {
                out.push(0);
            }
        }
        DataType::Links => {
            let mut lk_count = 0usize;
            let mut has_nulls = false;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        lk_count += lks.len();
                        if !has_nulls && lks.iter().any(|lk| lk.attributes.is_none()) {
                            has_nulls = true;
                        }
                    }
                    let u = checked_u32(lk_count, "links count exceeds u32::MAX")?;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            let lk_u32 = checked_u32(lk_count, "links count exceeds u32::MAX")?;
            out.extend_from_slice(&lk_u32.to_le_bytes());

            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        for lk in lks {
                            out.extend_from_slice(&lk.trace_id);
                        }
                    }
                }
            }
            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        for lk in lks {
                            out.extend_from_slice(&lk.span_id);
                        }
                    }
                }
            }

            // attributes
            let mut a_off = 0usize;
            out.extend_from_slice(&0u32.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        for lk in lks {
                            if let Some(attr) = &lk.attributes {
                                a_off += attr.len();
                            }
                            let u = checked_u32(a_off, "utf8 bytes length exceeds u32::MAX")?;
                            out.extend_from_slice(&u.to_le_bytes());
                        }
                    }
                }
            }
            let a_len = checked_u32(a_off, "utf8 bytes length exceeds u32::MAX")?;
            out.extend_from_slice(&a_len.to_le_bytes());
            for g in groups {
                for v in *g {
                    if let ScalarValue::Links(lks) = v {
                        for lk in lks {
                            if let Some(attr) = &lk.attributes {
                                out.extend_from_slice(attr.as_bytes());
                            }
                        }
                    }
                }
            }

            if has_nulls {
                out.push(1);
                let bm_start = out.len();
                out.resize(bm_start + lk_count.div_ceil(8), 0);
                let mut i = 0usize;
                for g in groups {
                    for v in *g {
                        if let ScalarValue::Links(lks) = v {
                            for lk in lks {
                                if lk.attributes.is_some() {
                                    out[bm_start + i / 8] |= 1u8 << (i % 8);
                                }
                                i += 1;
                            }
                        }
                    }
                }
            } else {
                out.push(0);
            }
        }
    }
    Ok(())
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
                write_scalar_groups_flat_data(
                    out,
                    &value.data_type(),
                    &[std::slice::from_ref(value)],
                )?;
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

fn checked_budget_add(acc: usize, add: usize, max_bytes: usize) -> Result<usize, ColumnError> {
    acc.checked_add(add)
        .filter(|&sum| sum <= max_bytes)
        .ok_or(ColumnError::InvalidFrame {
            reason: "const scalar exceeds decoded size limit",
        })
}

fn checked_budget_mul(count: usize, elem: usize, max_bytes: usize) -> Result<usize, ColumnError> {
    count
        .checked_mul(elem)
        .filter(|&prod| prod <= max_bytes)
        .ok_or(ColumnError::InvalidFrame {
            reason: "const scalar exceeds decoded size limit",
        })
}

fn is_zero_heap_flat_data(data: &FlatData) -> bool {
    matches!(
        data,
        FlatData::Null(_)
            | FlatData::Bool(_)
            | FlatData::I64(_)
            | FlatData::F64(_)
            | FlatData::Decimal128 { .. }
            | FlatData::TimestampNs { .. }
            | FlatData::Date(_)
            | FlatData::Interval(_)
            | FlatData::FixedBytes8(_)
            | FlatData::FixedBytes16(_)
    )
}

fn estimate_row0_scalar_heap_bytes(
    col: &Column,
    row: usize,
    max_bytes: usize,
) -> Result<usize, ColumnError> {
    match col {
        Column::Flat(fc) => estimate_flat_row_heap_bytes(fc, row, max_bytes),
        Column::Const { value, len } => {
            if row >= *len {
                return Err(ColumnError::InvalidBatch {
                    reason: "row index out of bounds",
                });
            }
            checked_budget_add(0, value.heap_bytes(), max_bytes)
        }
        Column::Dict(d) => {
            if row >= d.len() {
                return Err(ColumnError::InvalidBatch {
                    reason: "row index out of bounds",
                });
            }
            if d.nulls().is_some_and(|bm| !bm.get(row)) {
                return Ok(0);
            }
            estimate_flat_row_heap_bytes(d.values(), d.codes()[row] as usize, max_bytes)
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return Err(ColumnError::InvalidBatch {
                    reason: "row index out of bounds",
                });
            }
            let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
            estimate_flat_row_heap_bytes(r.values(), run_idx, max_bytes)
        }
    }
}

fn estimate_flat_row_heap_bytes(
    flat: &FlatColumn,
    row: usize,
    max_bytes: usize,
) -> Result<usize, ColumnError> {
    if row >= flat.len() {
        return Err(ColumnError::InvalidBatch {
            reason: "row index out of bounds",
        });
    }
    if flat.is_null(row) {
        return Ok(0);
    }
    match flat.data() {
        d if is_zero_heap_flat_data(d) => Ok(0),
        FlatData::Utf8(u) => {
            let len = (u.offsets()[row + 1] - u.offsets()[row]) as usize;
            checked_budget_add(0, len, max_bytes)
        }
        FlatData::Binary(b) => {
            let len = (b.offsets()[row + 1] - b.offsets()[row]) as usize;
            checked_budget_add(0, len, max_bytes)
        }
        FlatData::MapUtf8(m) => {
            let start = m.offsets()[row] as usize;
            let end = m.offsets()[row + 1] as usize;
            let count = end - start;
            let mut total =
                checked_budget_mul(count, size_of::<(String, Option<String>)>(), max_bytes)?;
            let key_bytes = (m.keys().offsets()[end] - m.keys().offsets()[start]) as usize;
            total = checked_budget_add(total, key_bytes, max_bytes)?;
            if let Some(vn) = m.value_nulls() {
                for idx in start..end {
                    if vn.get(idx) {
                        let v_len =
                            (m.values().offsets()[idx + 1] - m.values().offsets()[idx]) as usize;
                        total = checked_budget_add(total, v_len, max_bytes)?;
                    }
                }
            } else {
                let val_bytes = (m.values().offsets()[end] - m.values().offsets()[start]) as usize;
                total = checked_budget_add(total, val_bytes, max_bytes)?;
            }
            Ok(total)
        }
        FlatData::Events(e) => {
            let start = e.offsets()[row] as usize;
            let end = e.offsets()[row + 1] as usize;
            let count = end - start;
            let mut total = checked_budget_mul(count, size_of::<EventItem>(), max_bytes)?;
            let name_bytes = (e.names().offsets()[end] - e.names().offsets()[start]) as usize;
            total = checked_budget_add(total, name_bytes, max_bytes)?;
            if let Some(an) = e.attribute_nulls() {
                for idx in start..end {
                    if an.get(idx) {
                        let a_len = (e.attributes().offsets()[idx + 1]
                            - e.attributes().offsets()[idx])
                            as usize;
                        total = checked_budget_add(total, a_len, max_bytes)?;
                    }
                }
            } else {
                let attr_bytes =
                    (e.attributes().offsets()[end] - e.attributes().offsets()[start]) as usize;
                total = checked_budget_add(total, attr_bytes, max_bytes)?;
            }
            Ok(total)
        }
        FlatData::Links(l) => {
            let start = l.offsets()[row] as usize;
            let end = l.offsets()[row + 1] as usize;
            let count = end - start;
            let mut total = checked_budget_mul(count, size_of::<LinkItem>(), max_bytes)?;
            if let Some(an) = l.attribute_nulls() {
                for idx in start..end {
                    if an.get(idx) {
                        let a_len = (l.attributes().offsets()[idx + 1]
                            - l.attributes().offsets()[idx])
                            as usize;
                        total = checked_budget_add(total, a_len, max_bytes)?;
                    }
                }
            } else {
                let attr_bytes =
                    (l.attributes().offsets()[end] - l.attributes().offsets()[start]) as usize;
                total = checked_budget_add(total, attr_bytes, max_bytes)?;
            }
            Ok(total)
        }
        FlatData::List {
            offsets, values, ..
        } => {
            let start = offsets[row] as usize;
            let end = offsets[row + 1] as usize;
            let len = end - start;
            let mut total = checked_budget_mul(len, size_of::<ScalarValue>(), max_bytes)?;
            if len == 0 {
                return Ok(total);
            }
            match values.as_ref() {
                Column::Flat(fc) => {
                    if is_zero_heap_flat_data(fc.data()) {
                        return Ok(total);
                    }
                    for idx in start..end {
                        let elem = estimate_flat_row_heap_bytes(fc, idx, max_bytes - total)?;
                        total += elem;
                    }
                }
                Column::Const {
                    value,
                    len: col_len,
                } => {
                    if end > *col_len {
                        return Err(ColumnError::InvalidBatch {
                            reason: "list offsets out of bounds",
                        });
                    }
                    let single = value.heap_bytes();
                    let child_bytes = checked_budget_mul(len, single, max_bytes - total)?;
                    total += child_bytes;
                }
                Column::Dict(d) => {
                    if end > d.len() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "list offsets out of bounds",
                        });
                    }
                    if is_zero_heap_flat_data(d.values().data()) {
                        return Ok(total);
                    }
                    for idx in start..end {
                        let elem = estimate_row0_scalar_heap_bytes(values, idx, max_bytes - total)?;
                        total += elem;
                    }
                }
                Column::Rle(r) => {
                    if end > r.len() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "list offsets out of bounds",
                        });
                    }
                    if is_zero_heap_flat_data(r.values().data()) {
                        return Ok(total);
                    }
                    let mut run_idx = r.run_ends().partition_point(|&re| (re as usize) <= start);
                    let mut cur = start;
                    while cur < end {
                        let run_end = (r.run_ends()[run_idx] as usize).min(end);
                        let run_len = run_end - cur;
                        let single =
                            estimate_flat_row_heap_bytes(r.values(), run_idx, max_bytes - total)?;
                        let run_bytes = checked_budget_mul(run_len, single, max_bytes - total)?;
                        total += run_bytes;
                        cur = run_end;
                        run_idx += 1;
                    }
                }
            }
            Ok(total)
        }
        FlatData::Null(_)
        | FlatData::Bool(_)
        | FlatData::I64(_)
        | FlatData::F64(_)
        | FlatData::Decimal128 { .. }
        | FlatData::TimestampNs { .. }
        | FlatData::Date(_)
        | FlatData::Interval(_)
        | FlatData::FixedBytes8(_)
        | FlatData::FixedBytes16(_) => Ok(0),
    }
}

fn check_mat_add(acc: usize, add: usize, budget: usize) -> Result<usize, ColumnError> {
    acc.checked_add(add)
        .filter(|&sum| sum <= budget)
        .ok_or(ColumnError::InvalidFrame {
            reason: "decoded materialized size exceeds max_decoded",
        })
}

fn check_mat_mul(count: usize, elem: usize, budget: usize) -> Result<usize, ColumnError> {
    count
        .checked_mul(elem)
        .filter(|&prod| prod <= budget)
        .ok_or(ColumnError::InvalidFrame {
            reason: "decoded materialized size exceeds max_decoded",
        })
}

fn scalar_materialized_bytes(value: &ScalarValue, budget: usize) -> Result<usize, ColumnError> {
    let bytes = match value {
        ScalarValue::Null(_) | ScalarValue::Bool(_) => 1,
        ScalarValue::Date(_) => 4,
        ScalarValue::I64(_)
        | ScalarValue::F64(_)
        | ScalarValue::TimestampNs { .. }
        | ScalarValue::FixedBytes8(_) => 8,
        ScalarValue::Decimal128 { .. }
        | ScalarValue::Interval(_)
        | ScalarValue::FixedBytes16(_) => 16,
        ScalarValue::Utf8(s) => check_mat_add(4, s.len(), budget)?,
        ScalarValue::Binary(b) => check_mat_add(4, b.len(), budget)?,
        ScalarValue::List { values, .. } => {
            let mut total = 4usize;
            if total > budget {
                return Err(ColumnError::InvalidFrame {
                    reason: "decoded materialized size exceeds max_decoded",
                });
            }
            for v in values {
                let elem = scalar_materialized_bytes(v, budget - total)?;
                total += elem;
            }
            total
        }
        ScalarValue::MapUtf8(entries) => {
            let mut total = 4usize;
            if total > budget {
                return Err(ColumnError::InvalidFrame {
                    reason: "decoded materialized size exceeds max_decoded",
                });
            }
            for (k, v) in entries {
                let entry_bytes = 8usize
                    .checked_add(k.len())
                    .and_then(|s| s.checked_add(v.as_ref().map_or(0, String::len)))
                    .ok_or(ColumnError::InvalidFrame {
                        reason: "decoded materialized size exceeds max_decoded",
                    })?;
                total = check_mat_add(total, entry_bytes, budget)?;
            }
            total
        }
        ScalarValue::Events(evs) => {
            let mut total = 4usize;
            if total > budget {
                return Err(ColumnError::InvalidFrame {
                    reason: "decoded materialized size exceeds max_decoded",
                });
            }
            for ev in evs {
                let ev_bytes = 16usize
                    .checked_add(ev.name.len())
                    .and_then(|s| s.checked_add(ev.attributes.as_ref().map_or(0, String::len)))
                    .ok_or(ColumnError::InvalidFrame {
                        reason: "decoded materialized size exceeds max_decoded",
                    })?;
                total = check_mat_add(total, ev_bytes, budget)?;
            }
            total
        }
        ScalarValue::Links(lks) => {
            let mut total = 4usize;
            if total > budget {
                return Err(ColumnError::InvalidFrame {
                    reason: "decoded materialized size exceeds max_decoded",
                });
            }
            for lk in lks {
                let lk_bytes = 28usize
                    .checked_add(lk.attributes.as_ref().map_or(0, String::len))
                    .ok_or(ColumnError::InvalidFrame {
                        reason: "decoded materialized size exceeds max_decoded",
                    })?;
                total = check_mat_add(total, lk_bytes, budget)?;
            }
            total
        }
    };
    if bytes > budget {
        return Err(ColumnError::InvalidFrame {
            reason: "decoded materialized size exceeds max_decoded",
        });
    }
    Ok(bytes)
}

fn fixed_width_slot_bytes(data: &FlatData) -> Option<usize> {
    match data {
        FlatData::Null(_) | FlatData::Bool(_) => Some(1),
        FlatData::Date(_) => Some(4),
        FlatData::I64(_)
        | FlatData::F64(_)
        | FlatData::TimestampNs { .. }
        | FlatData::FixedBytes8(_) => Some(8),
        FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => Some(16),
        FlatData::Utf8(_)
        | FlatData::Binary(_)
        | FlatData::List { .. }
        | FlatData::MapUtf8(_)
        | FlatData::Events(_)
        | FlatData::Links(_) => None,
    }
}

fn flat_data_non_list_range_bytes(
    data: &FlatData,
    start: usize,
    end: usize,
    budget: usize,
) -> Result<usize, ColumnError> {
    let count = end - start;
    if count == 0 {
        return Ok(0);
    }
    match data {
        FlatData::Null(_) | FlatData::Bool(_) => check_mat_mul(count, 1, budget),
        FlatData::Date(_) => check_mat_mul(count, 4, budget),
        FlatData::I64(_)
        | FlatData::F64(_)
        | FlatData::TimestampNs { .. }
        | FlatData::FixedBytes8(_) => check_mat_mul(count, 8, budget),
        FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
            check_mat_mul(count, 16, budget)
        }
        FlatData::Utf8(u) => {
            let off_bytes = check_mat_mul(count, 4, budget)?;
            let str_bytes = (u.offsets()[end] - u.offsets()[start]) as usize;
            check_mat_add(off_bytes, str_bytes, budget)
        }
        FlatData::Binary(b) => {
            let off_bytes = check_mat_mul(count, 4, budget)?;
            let bin_bytes = (b.offsets()[end] - b.offsets()[start]) as usize;
            check_mat_add(off_bytes, bin_bytes, budget)
        }
        FlatData::MapUtf8(m) => {
            let row_off = check_mat_mul(count, 4, budget)?;
            let e_start = m.offsets()[start] as usize;
            let e_end = m.offsets()[end] as usize;
            let entries = e_end - e_start;
            let entry_off = check_mat_mul(entries, 8, budget)?;
            let key_bytes = (m.keys().offsets()[e_end] - m.keys().offsets()[e_start]) as usize;
            let val_bytes = (m.values().offsets()[e_end] - m.values().offsets()[e_start]) as usize;
            let total = check_mat_add(row_off, entry_off, budget)?;
            let total = check_mat_add(total, key_bytes, budget)?;
            check_mat_add(total, val_bytes, budget)
        }
        FlatData::Events(e) => {
            let row_off = check_mat_mul(count, 4, budget)?;
            let e_start = e.offsets()[start] as usize;
            let e_end = e.offsets()[end] as usize;
            let entries = e_end - e_start;
            let entry_fixed = check_mat_mul(entries, 16, budget)?;
            let name_bytes = (e.names().offsets()[e_end] - e.names().offsets()[e_start]) as usize;
            let attr_bytes =
                (e.attributes().offsets()[e_end] - e.attributes().offsets()[e_start]) as usize;
            let total = check_mat_add(row_off, entry_fixed, budget)?;
            let total = check_mat_add(total, name_bytes, budget)?;
            check_mat_add(total, attr_bytes, budget)
        }
        FlatData::Links(l) => {
            let row_off = check_mat_mul(count, 4, budget)?;
            let e_start = l.offsets()[start] as usize;
            let e_end = l.offsets()[end] as usize;
            let entries = e_end - e_start;
            let entry_fixed = check_mat_mul(entries, 28, budget)?;
            let attr_bytes =
                (l.attributes().offsets()[e_end] - l.attributes().offsets()[e_start]) as usize;
            let total = check_mat_add(row_off, entry_fixed, budget)?;
            check_mat_add(total, attr_bytes, budget)
        }
        FlatData::List { .. } => unreachable!(),
    }
}

const MEMO_ENTRY_BYTES: usize = 24;

struct MaterializedEstimator<'a> {
    const_cache: [Option<usize>; MAX_NESTING_DEPTH + 1],
    memo: [HashMap<u32, usize>; MAX_NESTING_DEPTH],
    cap_charged: [usize; MAX_NESTING_DEPTH],
    reserved_bytes: usize,
    pool: Option<&'a mut Reservation>,
}

impl Drop for MaterializedEstimator<'_> {
    fn drop(&mut self) {
        if self.reserved_bytes > 0
            && let Some(res) = self.pool.as_deref_mut()
        {
            res.shrink(self.reserved_bytes);
            self.reserved_bytes = 0;
        }
    }
}

impl<'a> MaterializedEstimator<'a> {
    fn new(pool: Option<&'a mut Reservation>) -> Self {
        Self {
            const_cache: [None; MAX_NESTING_DEPTH + 1],
            memo: std::array::from_fn(|_| HashMap::new()),
            cap_charged: [0; MAX_NESTING_DEPTH],
            reserved_bytes: 0,
            pool,
        }
    }

    fn cache_insert(&mut self, depth: usize, key: u32, val: usize) -> Result<(), ColumnError> {
        if depth >= MAX_NESTING_DEPTH {
            return Err(ColumnError::InvalidFrame {
                reason: "list nesting depth exceeds maximum",
            });
        }
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.memo[depth].entry(key) {
            e.insert(val);
            return Ok(());
        }
        if self.memo[depth].len() == self.memo[depth].capacity() {
            let old_cap = self.memo[depth].capacity();
            let next_cap = if old_cap == 0 {
                4
            } else {
                old_cap.saturating_mul(2).saturating_add(1)
            };
            let target_charged =
                next_cap
                    .checked_mul(MEMO_ENTRY_BYTES)
                    .ok_or(ColumnError::Overflow {
                        reason: "materialized estimator memo reservation overflow",
                    })?;
            let prev_charged = self.cap_charged[depth];
            if target_charged > prev_charged {
                let delta = target_charged - prev_charged;
                if let Some(res) = self.pool.as_deref_mut() {
                    res.try_grow(delta)?;
                }
                self.cap_charged[depth] = target_charged;
                self.reserved_bytes = self.reserved_bytes.saturating_add(delta);
            }
            self.memo[depth].reserve(1);
        }
        self.memo[depth].insert(key, val);
        let actual_cap = self.memo[depth].capacity();
        let actual_charged =
            actual_cap
                .checked_mul(MEMO_ENTRY_BYTES)
                .ok_or(ColumnError::Overflow {
                    reason: "materialized estimator memo reservation overflow",
                })?;
        let cur_charged = self.cap_charged[depth];
        if actual_charged < cur_charged {
            let excess = cur_charged - actual_charged;
            if let Some(res) = self.pool.as_deref_mut() {
                res.shrink(excess);
            }
            self.cap_charged[depth] = actual_charged;
            self.reserved_bytes = self.reserved_bytes.saturating_sub(excess);
        } else if actual_charged > cur_charged {
            let extra = actual_charged - cur_charged;
            if let Some(res) = self.pool.as_deref_mut() {
                res.try_grow(extra)?;
            }
            self.cap_charged[depth] = actual_charged;
            self.reserved_bytes = self.reserved_bytes.saturating_add(extra);
        }
        Ok(())
    }

    fn estimate_column_range(
        &mut self,
        col: &Column,
        range: Range<usize>,
        depth: usize,
        remaining_budget: usize,
    ) -> Result<usize, ColumnError> {
        if range.start >= range.end {
            return Ok(0);
        }
        if depth > MAX_NESTING_DEPTH {
            return Err(ColumnError::InvalidFrame {
                reason: "list nesting depth exceeds maximum",
            });
        }
        match col {
            Column::Flat(fc) => match fc.data() {
                FlatData::List {
                    offsets, values, ..
                } => {
                    if depth >= MAX_NESTING_DEPTH {
                        return Err(ColumnError::InvalidFrame {
                            reason: "list nesting depth exceeds maximum",
                        });
                    }
                    let row_off = check_mat_mul(range.end - range.start, 4, remaining_budget)?;
                    let child_start = offsets[range.start] as usize;
                    let child_end = offsets[range.end] as usize;
                    let child_sz = self.estimate_column_range(
                        values,
                        child_start..child_end,
                        depth + 1,
                        remaining_budget - row_off,
                    )?;
                    Ok(row_off + child_sz)
                }
                data => {
                    flat_data_non_list_range_bytes(data, range.start, range.end, remaining_budget)
                }
            },
            Column::Const { value, .. } => {
                let count = range.end - range.start;
                let elem_bytes = match self.const_cache[depth] {
                    Some(sz) => sz,
                    None => {
                        let sz = scalar_materialized_bytes(value, remaining_budget / count)?;
                        self.const_cache[depth] = Some(sz);
                        sz
                    }
                };
                check_mat_mul(count, elem_bytes, remaining_budget)
            }
            Column::Dict(d) => {
                let data = d.values().data();
                if let Some(slot) = fixed_width_slot_bytes(data) {
                    return check_mat_mul(range.end - range.start, slot, remaining_budget);
                }
                let codes = d.codes();
                let nulls = d.nulls();
                let mut range_sum = 0usize;
                if let FlatData::List {
                    offsets, values, ..
                } = data
                {
                    if depth >= MAX_NESTING_DEPTH {
                        return Err(ColumnError::InvalidFrame {
                            reason: "list nesting depth exceeds maximum",
                        });
                    }
                    let should_memo = depth > 0 || (range.end - range.start) > 1;
                    for row in range {
                        if nulls.is_some_and(|bm| !bm.get(row)) {
                            range_sum = check_mat_add(range_sum, 4, remaining_budget)?;
                            continue;
                        }
                        let code = codes[row];
                        let elem = if let Some(&cached) = self.memo[depth].get(&code) {
                            cached
                        } else {
                            let code_idx = code as usize;
                            let entry_budget = remaining_budget - range_sum;
                            let row_off = check_mat_add(0, 4, entry_budget)?;
                            let child_start = offsets[code_idx] as usize;
                            let child_end = offsets[code_idx + 1] as usize;
                            let child_sz = self.estimate_column_range(
                                values,
                                child_start..child_end,
                                depth + 1,
                                entry_budget - row_off,
                            )?;
                            let sz = row_off + child_sz;
                            if should_memo {
                                self.cache_insert(depth, code, sz)?;
                            }
                            sz
                        };
                        range_sum = check_mat_add(range_sum, elem, remaining_budget)?;
                    }
                } else {
                    for row in range {
                        if nulls.is_some_and(|bm| !bm.get(row)) {
                            range_sum = check_mat_add(range_sum, 4, remaining_budget)?;
                        } else {
                            let code_idx = codes[row] as usize;
                            let elem = flat_data_non_list_range_bytes(
                                data,
                                code_idx,
                                code_idx + 1,
                                remaining_budget - range_sum,
                            )?;
                            range_sum += elem;
                        }
                    }
                }
                Ok(range_sum)
            }
            Column::Rle(r) => {
                let data = r.values().data();
                if let Some(slot) = fixed_width_slot_bytes(data) {
                    return check_mat_mul(range.end - range.start, slot, remaining_budget);
                }
                let run_ends = r.run_ends();
                let mut run_idx = run_ends.partition_point(|&e| (e as usize) <= range.start);
                let mut cur = range.start;
                let mut range_sum = 0usize;
                if let FlatData::List {
                    offsets, values, ..
                } = data
                {
                    if depth >= MAX_NESTING_DEPTH {
                        return Err(ColumnError::InvalidFrame {
                            reason: "list nesting depth exceeds maximum",
                        });
                    }
                    while cur < range.end {
                        if run_idx >= run_ends.len() {
                            return Err(ColumnError::InvalidFrame {
                                reason: "rle run_ends does not cover column range",
                            });
                        }
                        let run_end = (run_ends[run_idx] as usize).min(range.end);
                        if run_end <= cur {
                            return Err(ColumnError::InvalidFrame {
                                reason: "rle run_ends not strictly increasing",
                            });
                        }
                        let count = run_end - cur;
                        let run_key =
                            u32::try_from(run_idx).map_err(|_| ColumnError::Overflow {
                                reason: "rle run index exceeds u32::MAX",
                            })?;
                        let elem = if depth > 0
                            && let Some(&cached) = self.memo[depth].get(&run_key)
                        {
                            cached
                        } else {
                            let per_row_budget = (remaining_budget - range_sum) / count;
                            let row_off = check_mat_add(0, 4, per_row_budget)?;
                            let child_start = offsets[run_idx] as usize;
                            let child_end = offsets[run_idx + 1] as usize;
                            let child_sz = self.estimate_column_range(
                                values,
                                child_start..child_end,
                                depth + 1,
                                per_row_budget - row_off,
                            )?;
                            let sz = row_off + child_sz;
                            if depth > 0 {
                                self.cache_insert(depth, run_key, sz)?;
                            }
                            sz
                        };
                        let part = check_mat_mul(count, elem, remaining_budget - range_sum)?;
                        range_sum += part;
                        cur = run_end;
                        run_idx += 1;
                    }
                } else {
                    while cur < range.end {
                        if run_idx >= run_ends.len() {
                            return Err(ColumnError::InvalidFrame {
                                reason: "rle run_ends does not cover column range",
                            });
                        }
                        let run_end = (run_ends[run_idx] as usize).min(range.end);
                        if run_end <= cur {
                            return Err(ColumnError::InvalidFrame {
                                reason: "rle run_ends not strictly increasing",
                            });
                        }
                        let count = run_end - cur;
                        let per_row_budget = (remaining_budget - range_sum) / count;
                        let elem = flat_data_non_list_range_bytes(
                            data,
                            run_idx,
                            run_idx + 1,
                            per_row_budget,
                        )?;
                        let part = check_mat_mul(count, elem, remaining_budget - range_sum)?;
                        range_sum += part;
                        cur = run_end;
                        run_idx += 1;
                    }
                }
                Ok(range_sum)
            }
        }
    }
}

fn estimate_column_materialized_bytes(
    col: &Column,
    remaining_budget: usize,
    pool: Option<&mut Reservation>,
) -> Result<usize, ColumnError> {
    let mut estimator = MaterializedEstimator::new(pool);
    estimator.estimate_column_range(col, 0..col.len(), 0, remaining_budget)
}

fn validate_scalar_logical_elements(
    value: &ScalarValue,
    depth: usize,
    max_decoded: u32,
    logical_elements: &mut u64,
) -> Result<(), ColumnError> {
    if let ScalarValue::List { values, .. } = value {
        if depth >= MAX_NESTING_DEPTH {
            return Err(ColumnError::InvalidFrame {
                reason: "list nesting depth exceeds maximum",
            });
        }
        let child_len = checked_u32(values.len(), "list child length exceeds u32::MAX")?;
        let _ = add_logical_elements(
            logical_elements,
            child_len,
            max_decoded,
            "list child length exceeds max_decoded",
        )?;
        for elem in values {
            validate_scalar_logical_elements(elem, depth + 1, max_decoded, logical_elements)?;
        }
    }
    Ok(())
}

fn validate_flat_column_encode_limits(
    fc: &FlatColumn,
    depth: usize,
    max_decoded: u32,
    logical_elements: &mut u64,
) -> Result<(), ColumnError> {
    if let FlatData::List { values, .. } = fc.data() {
        if depth >= MAX_NESTING_DEPTH {
            return Err(ColumnError::InvalidFrame {
                reason: "list nesting depth exceeds maximum",
            });
        }
        let child_len = checked_u32(values.len(), "list child length exceeds u32::MAX")?;
        let _ = add_logical_elements(
            logical_elements,
            child_len,
            max_decoded,
            "list child length exceeds max_decoded",
        )?;
        validate_column_encode_limits(values, depth + 1, max_decoded, logical_elements)?;
    }
    Ok(())
}

fn validate_column_encode_limits(
    col: &Column,
    depth: usize,
    max_decoded: u32,
    logical_elements: &mut u64,
) -> Result<(), ColumnError> {
    match col {
        Column::Flat(fc) => {
            validate_flat_column_encode_limits(fc, depth, max_decoded, logical_elements)
        }
        Column::Const { value, .. } => {
            if !value.is_null() {
                if value.heap_bytes() > max_decoded as usize {
                    return Err(ColumnError::InvalidFrame {
                        reason: "const scalar exceeds decoded size limit",
                    });
                }
                validate_scalar_logical_elements(value, depth, max_decoded, logical_elements)?;
            }
            Ok(())
        }
        Column::Dict(d) => {
            let dict_len = checked_u32(d.values().len(), "dict length exceeds u32::MAX")?;
            let _ = add_logical_elements(
                logical_elements,
                dict_len,
                max_decoded,
                "dictionary length exceeds max_decoded",
            )?;
            validate_flat_column_encode_limits(d.values(), depth, max_decoded, logical_elements)
        }
        Column::Rle(r) => {
            let run_count = r.values().len();
            if run_count > r.len() {
                return Err(ColumnError::InvalidFrame {
                    reason: "rle run_count exceeds column length",
                });
            }
            if !r.is_empty() && run_count == 0 {
                return Err(ColumnError::InvalidFrame {
                    reason: "non-empty rle column has zero runs",
                });
            }
            let run_count_u32 = checked_u32(run_count, "rle run count exceeds u32::MAX")?;
            let _ = add_logical_elements(
                logical_elements,
                run_count_u32,
                max_decoded,
                "rle run_count exceeds max_decoded",
            )?;
            validate_flat_column_encode_limits(r.values(), depth, max_decoded, logical_elements)
        }
    }
}

fn validate_batch_decode_limits(
    batch: &Batch,
    max_decoded: u32,
    mut pool: Option<&mut Reservation>,
) -> Result<(), ColumnError> {
    let raw_len_u32 = checked_u32(batch.raw_len(), "batch row count exceeds u32::MAX")?;
    if raw_len_u32 > max_decoded {
        return Err(ColumnError::InvalidFrame {
            reason: "batch raw_len exceeds max_decoded",
        });
    }
    let mut logical_elements = u64::from(raw_len_u32);
    let max_decoded_usize = max_decoded as usize;
    let mut materialized_bytes = 0usize;
    for col in batch.columns() {
        validate_column_encode_limits(col, 0, max_decoded, &mut logical_elements)?;
        let col_bytes = estimate_column_materialized_bytes(
            col,
            max_decoded_usize - materialized_bytes,
            pool.as_deref_mut(),
        )?;
        materialized_bytes += col_bytes;
    }
    Ok(())
}

fn add_logical_elements(
    logical_elements: &mut u64,
    add: u32,
    max_decoded: u32,
    reason: &'static str,
) -> Result<usize, ColumnError> {
    let next = logical_elements
        .checked_add(u64::from(add))
        .ok_or(ColumnError::InvalidFrame { reason })?;
    if next > u64::from(max_decoded) {
        return Err(ColumnError::InvalidFrame { reason });
    }
    *logical_elements = next;
    Ok(add as usize)
}

fn read_flat_data(
    reader: &mut BodyReader<'_>,
    data_type: &DataType,
    len: usize,
    depth: usize,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
    logical_elements: &mut u64,
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
            let child_len_u32 = reader.read_u32()?;
            let child_len = add_logical_elements(
                logical_elements,
                child_len_u32,
                max_decoded,
                "list child length exceeds max_decoded",
            )?;
            let values = read_column(
                reader,
                inner,
                child_len,
                depth + 1,
                pool,
                max_decoded,
                logical_elements,
            )?;
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
    max_decoded: u32,
    logical_elements: &mut u64,
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
    let data = read_flat_data(
        reader,
        data_type,
        len,
        depth,
        pool,
        max_decoded,
        logical_elements,
    )?;
    FlatColumn::try_new(data, nulls)
}

fn read_column(
    reader: &mut BodyReader<'_>,
    data_type: &DataType,
    len: usize,
    depth: usize,
    mut pool: Option<&mut Reservation>,
    max_decoded: u32,
    logical_elements: &mut u64,
) -> Result<Column, ColumnError> {
    let encoding_tag = reader.read_u8()?;
    match encoding_tag {
        0 => Ok(Column::Flat(read_flat_column(
            reader,
            data_type,
            len,
            depth,
            pool,
            max_decoded,
            logical_elements,
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
                let before_const = pool.as_ref().map_or(0, |p| p.size());
                let data = read_flat_data(
                    reader,
                    data_type,
                    1,
                    depth,
                    pool.as_deref_mut(),
                    max_decoded,
                    logical_elements,
                )?;
                let scalar = match data {
                    FlatData::Utf8(u) => {
                        if u.len() != 1 {
                            return Err(ColumnError::InvalidFrame {
                                reason: "const utf8 column length mismatch",
                            });
                        }
                        let bytes = u.into_bytes();
                        if bytes.len() > max_decoded as usize {
                            return Err(ColumnError::InvalidFrame {
                                reason: "const scalar exceeds decoded size limit",
                            });
                        }
                        let s = String::from_utf8(bytes).map_err(|_| ColumnError::InvalidUtf8)?;
                        ScalarValue::Utf8(s)
                    }
                    FlatData::Binary(b) => {
                        if b.len() != 1 {
                            return Err(ColumnError::InvalidFrame {
                                reason: "const binary column length mismatch",
                            });
                        }
                        let bytes = b.into_bytes();
                        if bytes.len() > max_decoded as usize {
                            return Err(ColumnError::InvalidFrame {
                                reason: "const scalar exceeds decoded size limit",
                            });
                        }
                        ScalarValue::Binary(bytes)
                    }
                    other => {
                        let flat = FlatColumn::try_new(other, None)?;
                        let estimated_heap =
                            estimate_flat_row_heap_bytes(&flat, 0, max_decoded as usize)?;
                        if let Some(res) = pool.as_deref_mut() {
                            res.try_grow(estimated_heap)?;
                        }
                        let scalar = flat.get_scalar(0)?;
                        drop(flat);
                        scalar
                    }
                };
                reconcile_reservation(pool, before_const, scalar.heap_bytes())?;
                Column::try_new_const(scalar, len)
            }
        }
        2 => {
            let dict_len_u32 = reader.read_u32()?;
            let dict_len = add_logical_elements(
                logical_elements,
                dict_len_u32,
                max_decoded,
                "dictionary length exceeds max_decoded",
            )?;
            let values = read_flat_column(
                reader,
                data_type,
                dict_len,
                depth,
                pool.as_deref_mut(),
                max_decoded,
                logical_elements,
            )?;
            let codes = reader.read_u32_vec(len, pool.as_deref_mut())?;
            let nulls = reader.read_optional_bitmap(len, pool)?;
            Ok(Column::Dict(DictColumn::try_new(values, codes, nulls)?))
        }
        3 => {
            let run_count_u32 = reader.read_u32()?;
            if (run_count_u32 as usize) > len {
                return Err(ColumnError::InvalidFrame {
                    reason: "rle run_count exceeds column length",
                });
            }
            if len > 0 && run_count_u32 == 0 {
                return Err(ColumnError::InvalidFrame {
                    reason: "non-empty rle column has zero runs",
                });
            }
            let run_count = add_logical_elements(
                logical_elements,
                run_count_u32,
                max_decoded,
                "rle run_count exceeds max_decoded",
            )?;
            let values = read_flat_column(
                reader,
                data_type,
                run_count,
                depth,
                pool.as_deref_mut(),
                max_decoded,
                logical_elements,
            )?;
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
        COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, GlobalPool, MemoryLimitKind, Selection,
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
        encode_batch(&batch, &mut buf, None).unwrap();
        let decoded = decode_batch(&buf, Some(batch.schema()), None).unwrap();
        assert!(batch.semantic_eq(&decoded));
        assert!(matches!(*decoded.columns()[17], Column::Const { .. }));
        assert!(matches!(*decoded.columns()[18], Column::Const { .. }));
        assert!(matches!(*decoded.columns()[19], Column::Dict(_)));
        assert!(matches!(*decoded.columns()[20], Column::Rle(_)));

        // Active Selection::Mask is compacted on encode
        let filtered = batch
            .filter_mask(&Bitmap::from_bools(&[true, false, true, false]))
            .unwrap();
        let mut f_buf = Vec::new();
        encode_batch(&filtered, &mut f_buf, None).unwrap();
        let f_decoded = decode_batch(&f_buf, Some(filtered.schema()), None).unwrap();
        assert_eq!(f_decoded.num_rows(), 2);
        assert_eq!(f_decoded.raw_len(), 2);
        assert_eq!(f_decoded.selection(), &Selection::All);
        assert!(filtered.semantic_eq(&f_decoded));

        // Empty batch (0 rows) round-trip
        let empty = Batch::empty(all_types_schema());
        let mut e_buf = Vec::new();
        encode_batch(&empty, &mut e_buf, None).unwrap();
        let e_decoded = decode_batch(&e_buf, Some(empty.schema()), None).unwrap();
        assert!(empty.semantic_eq(&e_decoded));
    }

    #[test]
    fn multi_frame_stream_and_torn_tail() {
        let b1 = all_types_batch();
        let b2 = b1.slice(1, 2, None).unwrap();
        let b3 = Batch::empty(all_types_schema());

        let mut stream = Vec::new();
        encode_batch(&b1, &mut stream, None).unwrap();
        let f1_end = stream.len();
        encode_batch(&b2, &mut stream, None).unwrap();
        let f2_end = stream.len();
        encode_batch(&b3, &mut stream, None).unwrap();
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
        encode_batch(&batch, &mut frame, None).unwrap();

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
            encode_batch_with_limit(&batch, &mut out, None, 16),
            Err(ColumnError::InvalidFrame { .. })
        ));
        assert!(out.is_empty());

        encode_batch(&batch, &mut out, None).unwrap();
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
        encode_batch(&batch, &mut buf, None).unwrap();

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
        encode_batch(&batch, &mut frame, None).unwrap();

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
            encode_batch(&batch, &mut framed, None).unwrap();

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
            // Pool size must be exact (0 if Err, memory_bytes if somehow valid)
            match decode_batch(&garbage, None, Some(&mut err_res)) {
                Ok(b) => prop_assert_eq!(err_res.size(), b.memory_bytes()),
                Err(_) => prop_assert_eq!(err_res.size(), 0),
            }
        }
    }

    fn wrap_uncompressed_body(body: &[u8]) -> Vec<u8> {
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
    }

    fn write_body_header(body: &mut Vec<u8>, raw_len: u32, col_name: &str, dt: &DataType) {
        body.extend_from_slice(&raw_len.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // col_count = 1
        body.extend_from_slice(&(col_name.len() as u32).to_le_bytes());
        body.extend_from_slice(col_name.as_bytes());
        body.push(1); // nullable = true
        write_data_type(body, dt);
    }

    #[test]
    fn estimate_row0_scalar_heap_bytes_matches_get_scalar() {
        let batch = all_types_batch();
        for col in batch.columns() {
            for row in 0..batch.raw_len() {
                let expected = col.get_scalar(row).unwrap().heap_bytes();
                let estimated = estimate_row0_scalar_heap_bytes(col, row, usize::MAX).unwrap();
                assert_eq!(estimated, expected);
            }
        }
    }

    #[test]
    fn const_list_null_huge_child_len_rejected_without_oom() {
        let dt = DataType::List(Box::new(DataType::Null));
        let mut body = Vec::new();
        write_body_header(&mut body, 1, "a", &dt);
        body.push(1); // encoding = Const
        body.push(0); // is_null = false
        body.extend_from_slice(&0u32.to_le_bytes()); // offsets[0]
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // offsets[1]
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len
        body.push(0); // child encoding = Flat
        body.push(0); // child has_nulls = false (Null data writes 0 bytes)

        let frame = wrap_uncompressed_body(&body);
        assert!(frame.len() <= 64, "crafted bomb frame should stay tiny");

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("const_null_bomb", true);
        let err = decode_batch(&frame, None, Some(&mut res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));
        assert_eq!(res.size(), 0);
    }

    #[test]
    fn const_list_with_const_or_rle_child_bomb_rejected() {
        // 1. Const(List(I64)) with Const child len = u32::MAX
        let dt_i64_list = DataType::List(Box::new(DataType::I64));
        let mut body_const = Vec::new();
        write_body_header(&mut body_const, 1, "a", &dt_i64_list);
        body_const.push(1); // encoding = Const
        body_const.push(0); // is_null = false
        body_const.extend_from_slice(&0u32.to_le_bytes());
        body_const.extend_from_slice(&u32::MAX.to_le_bytes());
        body_const.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len = u32::MAX
        body_const.push(1); // child encoding = Const
        body_const.push(0); // child is_null = false
        body_const.extend_from_slice(&42i64.to_le_bytes());

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("bomb_const_child", true);
        let err =
            decode_batch(&wrap_uncompressed_body(&body_const), None, Some(&mut res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));
        assert_eq!(res.size(), 0);

        // 2. Const(List(I64)) with Rle child run_ends = [u32::MAX]
        let mut body_rle = Vec::new();
        write_body_header(&mut body_rle, 1, "a", &dt_i64_list);
        body_rle.push(1); // encoding = Const
        body_rle.push(0); // is_null = false
        body_rle.extend_from_slice(&0u32.to_le_bytes());
        body_rle.extend_from_slice(&u32::MAX.to_le_bytes());
        body_rle.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len = u32::MAX
        body_rle.push(3); // child encoding = Rle
        body_rle.extend_from_slice(&1u32.to_le_bytes()); // run_count = 1
        body_rle.push(0); // values has_nulls = false
        body_rle.extend_from_slice(&42i64.to_le_bytes());
        body_rle.extend_from_slice(&u32::MAX.to_le_bytes()); // run_ends = [u32::MAX]

        let mut res_rle = scope.register("bomb_rle_child", true);
        let err =
            decode_batch(&wrap_uncompressed_body(&body_rle), None, Some(&mut res_rle)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));
        assert_eq!(res_rle.size(), 0);

        // 3. Const(List(Utf8)) with Const and Rle children where vec header fits cap but string bytes exceed cap
        let dt_utf8_list = DataType::List(Box::new(DataType::Utf8));
        let mut body_utf8_const = Vec::new();
        write_body_header(&mut body_utf8_const, 1, "a", &dt_utf8_list);
        body_utf8_const.push(1); // Const
        body_utf8_const.push(0);
        body_utf8_const.extend_from_slice(&0u32.to_le_bytes());
        body_utf8_const.extend_from_slice(&100u32.to_le_bytes());
        body_utf8_const.extend_from_slice(&100u32.to_le_bytes());
        body_utf8_const.push(1); // Const child
        body_utf8_const.push(0);
        body_utf8_const.extend_from_slice(&0u32.to_le_bytes());
        body_utf8_const.extend_from_slice(&64u32.to_le_bytes());
        body_utf8_const.extend_from_slice(&64u32.to_le_bytes());
        body_utf8_const.extend(std::iter::repeat_n(b'x', 64));

        let err =
            decode_batch_with_limit(&wrap_uncompressed_body(&body_utf8_const), None, None, 8192)
                .unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "const scalar exceeds decoded size limit"
            }
        ));

        let mut body_utf8_rle = Vec::new();
        write_body_header(&mut body_utf8_rle, 1, "a", &dt_utf8_list);
        body_utf8_rle.push(1); // Const
        body_utf8_rle.push(0);
        body_utf8_rle.extend_from_slice(&0u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&100u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&100u32.to_le_bytes());
        body_utf8_rle.push(3); // Rle child
        body_utf8_rle.extend_from_slice(&2u32.to_le_bytes()); // run_count = 2
        body_utf8_rle.push(0); // has_nulls = false
        body_utf8_rle.extend_from_slice(&0u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&64u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&128u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&128u32.to_le_bytes());
        body_utf8_rle.extend(std::iter::repeat_n(b'y', 128));
        body_utf8_rle.extend_from_slice(&50u32.to_le_bytes());
        body_utf8_rle.extend_from_slice(&100u32.to_le_bytes());

        let err =
            decode_batch_with_limit(&wrap_uncompressed_body(&body_utf8_rle), None, None, 8192)
                .unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "const scalar exceeds decoded size limit"
            }
        ));

        // 4. Nested depth-8 Const(List(...)) exponential bomb
        let mut nested_dt = DataType::I64;
        for _ in 0..MAX_NESTING_DEPTH {
            nested_dt = DataType::List(Box::new(nested_dt));
        }
        let mut body_depth8 = Vec::new();
        write_body_header(&mut body_depth8, 1, "a", &nested_dt);
        for _ in 0..MAX_NESTING_DEPTH {
            body_depth8.push(1); // Const
            body_depth8.push(0); // is_null = false
            body_depth8.extend_from_slice(&0u32.to_le_bytes());
            body_depth8.extend_from_slice(&16u32.to_le_bytes());
            body_depth8.extend_from_slice(&16u32.to_le_bytes()); // child_len = 16
        }
        body_depth8.push(1); // leaf Const(I64(1))
        body_depth8.push(0);
        body_depth8.extend_from_slice(&1i64.to_le_bytes());

        let large_scope = GlobalPool::standalone(2usize << 30);
        let mut res_depth8 = large_scope.register("bomb_depth8", true);
        let err = decode_batch(
            &wrap_uncompressed_body(&body_depth8),
            None,
            Some(&mut res_depth8),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "const scalar exceeds decoded size limit"
            }
        ));
        assert_eq!(res_depth8.size(), 0);
    }

    #[test]
    fn const_list_uncompressed_reservation_growth_and_rollback() {
        let dt = DataType::List(Box::new(DataType::I64));
        let col = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: (1..=8).map(ScalarValue::I64).collect(),
            },
            4,
        )
        .unwrap();
        let expected_scalar_bytes = col.memory_bytes();
        let flat_wire_bytes = 2 * 4 + 8 * 8; // offsets(8) + i64(64) = 72
        assert!(expected_scalar_bytes > flat_wire_bytes);

        let schema = Schema::try_new_arc(vec![Field::new("xs", dt, false)]).unwrap();
        let batch = Batch::try_new_all(schema, vec![col], 4).unwrap();
        let mut body = Vec::new();
        write_body(&batch, &mut body).unwrap();
        let frame = wrap_uncompressed_body(&body);

        // Generous pool grows from flat_wire_bytes to exact decoded.memory_bytes()
        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("const_grow", true);
        res.try_grow(16).unwrap();
        let decoded = decode_batch(&frame, Some(batch.schema()), Some(&mut res)).unwrap();
        assert_eq!(res.size(), 16 + decoded.memory_bytes());
        assert!(batch.semantic_eq(&decoded));

        // Tight pool allows flat_wire_bytes (72) but rejects growth to expected_scalar_bytes
        let tight_scope = GlobalPool::standalone(16 + flat_wire_bytes);
        let mut tight_res = tight_scope.register("const_tight", true);
        tight_res.try_grow(16).unwrap();
        let err = decode_batch(&frame, None, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::MemoryLimit {
                kind: MemoryLimitKind::SpillShare,
                ..
            }
        ));
        assert_eq!(tight_res.size(), 16);
    }

    #[test]
    fn non_canonical_all_ones_null_bitmap_reconciles_reservation() {
        let mut body = Vec::new();
        write_body_header(&mut body, 8, "x", &DataType::I64);
        body.push(0); // encoding = Flat
        body.push(1); // has_nulls = true (non-canonical all-ones bitmap follows)
        body.push(0xFF); // 8 valid bits
        for i in 0i64..8 {
            body.extend_from_slice(&i.to_le_bytes());
        }
        let frame = wrap_uncompressed_body(&body);

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("non_canon", true);
        res.try_grow(24).unwrap();
        let decoded = decode_batch(&frame, None, Some(&mut res)).unwrap();
        assert!(!decoded.columns()[0].has_nulls());
        assert_eq!(decoded.memory_bytes(), 8 * 8);
        assert_eq!(res.size(), 24 + decoded.memory_bytes());
    }

    #[test]
    fn flat_list_and_top_level_raw_len_logical_bombs_rejected() {
        let scope = GlobalPool::standalone(1024 * 1024);

        // 1. 50-byte Flat(List(I64)) over Const(I64) with child_len = u32::MAX
        let dt_i64_list = DataType::List(Box::new(DataType::I64));
        let mut body_flat_const = Vec::new();
        write_body_header(&mut body_flat_const, 1, "a", &dt_i64_list);
        body_flat_const.push(0); // Flat
        body_flat_const.push(0); // has_nulls = false
        body_flat_const.extend_from_slice(&0u32.to_le_bytes());
        body_flat_const.extend_from_slice(&u32::MAX.to_le_bytes());
        body_flat_const.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len = u32::MAX
        body_flat_const.push(1); // child encoding = Const
        body_flat_const.push(0); // child is_null = false
        body_flat_const.extend_from_slice(&7i64.to_le_bytes());
        let frame_flat_const = wrap_uncompressed_body(&body_flat_const);
        assert!(frame_flat_const.len() <= 64);

        let mut res1 = scope.register("flat_const_bomb", true);
        let err1 = decode_batch(&frame_flat_const, None, Some(&mut res1)).unwrap_err();
        assert!(matches!(
            err1,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));
        assert_eq!(res1.size(), 0);

        // 2. Flat(List(I64)) over Rle with child_len = u32::MAX
        let mut body_flat_rle = Vec::new();
        write_body_header(&mut body_flat_rle, 1, "a", &dt_i64_list);
        body_flat_rle.push(0); // Flat
        body_flat_rle.push(0); // has_nulls = false
        body_flat_rle.extend_from_slice(&0u32.to_le_bytes());
        body_flat_rle.extend_from_slice(&u32::MAX.to_le_bytes());
        body_flat_rle.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len = u32::MAX
        body_flat_rle.push(3); // child encoding = Rle
        body_flat_rle.extend_from_slice(&1u32.to_le_bytes());
        body_flat_rle.push(0);
        body_flat_rle.extend_from_slice(&7i64.to_le_bytes());
        body_flat_rle.extend_from_slice(&u32::MAX.to_le_bytes());
        let frame_flat_rle = wrap_uncompressed_body(&body_flat_rle);
        assert!(frame_flat_rle.len() <= 80);

        let err2 = decode_batch(&frame_flat_rle, None, None).unwrap_err();
        assert!(matches!(
            err2,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));

        // 3. Flat(List(Null)) with child_len = u32::MAX
        let dt_null_list = DataType::List(Box::new(DataType::Null));
        let mut body_flat_null = Vec::new();
        write_body_header(&mut body_flat_null, 1, "a", &dt_null_list);
        body_flat_null.push(0); // Flat
        body_flat_null.push(0); // has_nulls = false
        body_flat_null.extend_from_slice(&0u32.to_le_bytes());
        body_flat_null.extend_from_slice(&u32::MAX.to_le_bytes());
        body_flat_null.extend_from_slice(&u32::MAX.to_le_bytes()); // child_len = u32::MAX
        body_flat_null.push(0); // child encoding = Flat(Null)
        body_flat_null.push(0); // child has_nulls = false
        let frame_flat_null = wrap_uncompressed_body(&body_flat_null);
        assert!(frame_flat_null.len() <= 64);

        let err3 = decode_batch(&frame_flat_null, None, None).unwrap_err();
        assert!(matches!(
            err3,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));

        // 4. 40-byte frame with top-level raw_len = u32::MAX over Const / Flat(Null)
        let mut body_raw_const = Vec::new();
        write_body_header(&mut body_raw_const, u32::MAX, "a", &DataType::I64);
        body_raw_const.push(1); // Const
        body_raw_const.push(0);
        body_raw_const.extend_from_slice(&9i64.to_le_bytes());
        let frame_raw_const = wrap_uncompressed_body(&body_raw_const);
        assert!(frame_raw_const.len() <= 48);

        let err4 = decode_batch(&frame_raw_const, None, None).unwrap_err();
        assert!(matches!(
            err4,
            ColumnError::InvalidFrame {
                reason: "batch raw_len exceeds max_decoded"
            }
        ));

        let mut body_raw_null = Vec::new();
        write_body_header(&mut body_raw_null, u32::MAX, "a", &DataType::Null);
        body_raw_null.push(0); // Flat(Null)
        body_raw_null.push(0);
        let frame_raw_null = wrap_uncompressed_body(&body_raw_null);
        assert!(frame_raw_null.len() <= 40);

        let err5 = decode_batch(&frame_raw_null, None, None).unwrap_err();
        assert!(matches!(
            err5,
            ColumnError::InvalidFrame {
                reason: "batch raw_len exceeds max_decoded"
            }
        ));

        // 5. Small max_decoded still accepts valid tiny Flat(Null) / Const batches
        let mut body_tiny = Vec::new();
        write_body_header(&mut body_tiny, 10, "a", &DataType::Null);
        body_tiny.push(0);
        body_tiny.push(0);
        let frame_tiny = wrap_uncompressed_body(&body_tiny);
        let decoded_tiny = decode_batch_with_limit(&frame_tiny, None, None, 64).unwrap();
        assert_eq!(decoded_tiny.num_rows(), 10);
    }

    #[test]
    fn lz4_trailing_bytes_after_eof_rejected() {
        let real_body = vec![7u8; 128];
        let mut enc = FrameEncoder::new(Vec::new());
        enc.write_all(&real_body).unwrap();
        let mut payload = enc.finish().unwrap();
        // Append trailing garbage after the LZ4 end-of-frame marker.
        payload.extend_from_slice(b"junk");

        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        hdr[0..4].copy_from_slice(&FRAME_MAGIC);
        hdr[4] = FRAME_VERSION;
        hdr[5] = FLAG_LZ4;
        hdr[8..12].copy_from_slice(&(real_body.len() as u32).to_le_bytes());
        hdr[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), &payload);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&hdr);
        frame.extend_from_slice(&payload);

        let err = decode_batch(&frame, None, None).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "trailing bytes after lz4 frame"
            }
        ));
    }

    #[test]
    fn encode_batch_reservation_accounting_and_rollback() {
        let batch = all_types_batch();
        let mut out = Vec::new();

        // Ample pool accounts during encoding and restores to pre-call size on Ok.
        let ample = GlobalPool::standalone(1024 * 1024);
        let mut res = ample.register("encode_ample", true);
        res.try_grow(64).unwrap();
        encode_batch(&batch, &mut out, Some(&mut res)).unwrap();
        assert_eq!(res.size(), 64);
        assert!(!out.is_empty());

        // Tight pool rejects with MemoryLimit, leaves `out` unchanged, and restores `pool`.
        let tight = GlobalPool::standalone(128);
        let mut tight_res = tight.register("encode_tight", true);
        tight_res.try_grow(32).unwrap();
        let mut out_err = vec![1, 2, 3];
        let err = encode_batch(&batch, &mut out_err, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 32);
        assert_eq!(out_err, vec![1, 2, 3]);
    }

    #[test]
    fn multiplicative_list_rle_dict_const_and_wide_bombs_rejected() {
        let scope = GlobalPool::standalone(1024 * 1024);

        // 1. 8-level nested List + Rle (~230-byte frame expanding to 10^24 elements) -> rejected!
        let mut nested_dt = DataType::I64;
        for _ in 0..MAX_NESTING_DEPTH {
            nested_dt = DataType::List(Box::new(nested_dt));
        }
        let mut body_rle = Vec::new();
        write_body_header(&mut body_rle, 1000, "a", &nested_dt);
        for _ in 0..MAX_NESTING_DEPTH {
            body_rle.push(3); // Rle
            body_rle.extend_from_slice(&1u32.to_le_bytes()); // run_count = 1
            body_rle.push(0); // values has_nulls = false
            body_rle.extend_from_slice(&0u32.to_le_bytes()); // offsets[0]
            body_rle.extend_from_slice(&1000u32.to_le_bytes()); // offsets[1]
            body_rle.extend_from_slice(&1000u32.to_le_bytes()); // child_len = 1000
        }
        body_rle.push(3); // leaf Rle(I64)
        body_rle.extend_from_slice(&1u32.to_le_bytes()); // run_count = 1
        body_rle.push(0); // has_nulls = false
        body_rle.extend_from_slice(&42i64.to_le_bytes());
        body_rle.extend_from_slice(&1000u32.to_le_bytes()); // run_ends = [1000]
        // Close the 8 outer Rle run_ends vectors
        for _ in 0..MAX_NESTING_DEPTH {
            body_rle.extend_from_slice(&1000u32.to_le_bytes());
        }
        let frame_rle = wrap_uncompressed_body(&body_rle);
        assert!(
            frame_rle.len() <= 260,
            "8-level Rle bomb frame should stay tiny, got {}",
            frame_rle.len()
        );
        let mut res_rle = scope.register("rle_bomb", true);
        let err_rle = decode_batch(&frame_rle, None, Some(&mut res_rle)).unwrap_err();
        assert!(matches!(
            err_rle,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_rle.size(), 0);

        // 2. 8-level nested List + Dict (dict_len = 1, codes = [0; 100]) -> rejected on both encode & decode!
        let mut col_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(FlatData::I64(vec![7]), None).unwrap(),
                vec![0u32; 100],
                None,
            )
            .unwrap(),
        );
        let mut cur_dt = DataType::I64;
        for _ in 0..MAX_NESTING_DEPTH {
            let list_fc = FlatColumn::try_new(
                FlatData::List {
                    item_type: cur_dt.clone(),
                    offsets: vec![0, 100],
                    values: Box::new(col_dict),
                },
                None,
            )
            .unwrap();
            col_dict = Column::Dict(DictColumn::try_new(list_fc, vec![0u32; 100], None).unwrap());
            cur_dt = DataType::List(Box::new(cur_dt));
        }
        let schema_dict = Schema::try_new_arc(vec![Field::new("d", cur_dt, false)]).unwrap();
        let batch_dict = Batch::try_new_all(schema_dict, vec![col_dict], 100).unwrap();
        let mut enc_out_dict = vec![0xEE];
        let mut res_enc_dict = scope.register("dict_enc_bomb", true);
        res_enc_dict.try_grow(16).unwrap();
        let err_enc_dict =
            encode_batch(&batch_dict, &mut enc_out_dict, Some(&mut res_enc_dict)).unwrap_err();
        assert!(matches!(
            err_enc_dict,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(enc_out_dict, vec![0xEE]);
        assert_eq!(res_enc_dict.size(), 16);

        let mut body_dict = Vec::new();
        write_body(&batch_dict, &mut body_dict).unwrap();
        let frame_dict = wrap_uncompressed_body(&body_dict);

        let mut res_dict = scope.register("dict_bomb", true);
        let err_dict = decode_batch(&frame_dict, None, Some(&mut res_dict)).unwrap_err();
        assert!(matches!(
            err_dict,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_dict.size(), 0);

        // 3. Flat(List(Utf8)) with offsets = [0, 1_000_000] over Const(Utf8("x" * 100_000)) -> rejected on both encode & decode!
        let big_str = "x".repeat(100_000);
        let list_const_utf8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1_000_000],
                    values: Box::new(
                        Column::try_new_const(ScalarValue::Utf8(big_str), 1_000_000).unwrap(),
                    ),
                },
                None,
            )
            .unwrap(),
        );
        let schema_list_const = Schema::try_new_arc(vec![Field::new(
            "l",
            DataType::List(Box::new(DataType::Utf8)),
            false,
        )])
        .unwrap();
        let batch_list_const =
            Batch::try_new_all(schema_list_const, vec![list_const_utf8], 1).unwrap();
        let mut enc_out_lc = vec![0xCC];
        let mut res_enc_lc = scope.register("list_const_enc_bomb", true);
        res_enc_lc.try_grow(24).unwrap();
        let err_enc_lc =
            encode_batch(&batch_list_const, &mut enc_out_lc, Some(&mut res_enc_lc)).unwrap_err();
        assert!(matches!(
            err_enc_lc,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(enc_out_lc, vec![0xCC]);
        assert_eq!(res_enc_lc.size(), 24);

        let mut body_list_const = Vec::new();
        write_body(&batch_list_const, &mut body_list_const).unwrap();
        let frame_list_const = wrap_uncompressed_body(&body_list_const);

        let mut res_list_const = scope.register("list_const_bomb", true);
        let err_list_const =
            decode_batch(&frame_list_const, None, Some(&mut res_list_const)).unwrap_err();
        assert!(matches!(
            err_list_const,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_list_const.size(), 0);

        // 4. Wide Const(I64) (raw_len = 1 << 20, 200 columns -> 1600 MiB > 1 GiB) -> rejected on both encode & decode!
        let wide_raw_len = 1usize << 20;
        let wide_fields: Vec<Field> = (0..200)
            .map(|i| Field::new(format!("c{i}"), DataType::I64, false))
            .collect();
        let wide_cols: Vec<Column> = (0..200)
            .map(|i| Column::try_new_const(ScalarValue::I64(i), wide_raw_len).unwrap())
            .collect();
        let wide_schema = Schema::try_new_arc(wide_fields).unwrap();
        let wide_batch = Batch::try_new_all(wide_schema, wide_cols, wide_raw_len).unwrap();
        let mut enc_out_wide = vec![0xDD];
        let mut res_enc_wide = scope.register("wide_const_enc_bomb", true);
        res_enc_wide.try_grow(32).unwrap();
        let err_enc_wide =
            encode_batch(&wide_batch, &mut enc_out_wide, Some(&mut res_enc_wide)).unwrap_err();
        assert!(matches!(
            err_enc_wide,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(enc_out_wide, vec![0xDD]);
        assert_eq!(res_enc_wide.size(), 32);

        let mut body_wide = Vec::new();
        write_body(&wide_batch, &mut body_wide).unwrap();
        let wide_frame = wrap_uncompressed_body(&body_wide);

        let mut res_wide = scope.register("wide_const_bomb", true);
        let err_wide = decode_batch(&wide_frame, None, Some(&mut res_wide)).unwrap_err();
        assert!(matches!(
            err_wide,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_wide.size(), 0);
    }

    #[test]
    fn lz4_truncated_endmark_and_trailing_garbage_rejected() {
        let real_body = vec![9u8; 256];
        let mut enc = FrameEncoder::new(Vec::new());
        enc.write_all(&real_body).unwrap();
        let valid_payload = enc.finish().unwrap();
        assert!(valid_payload.ends_with(&[0, 0, 0, 0]));

        let frame_with_lz4_payload = |payload: &[u8]| -> Vec<u8> {
            let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
            let mut hdr = [0u8; FRAME_HEADER_LEN];
            hdr[0..4].copy_from_slice(&FRAME_MAGIC);
            hdr[4] = FRAME_VERSION;
            hdr[5] = FLAG_LZ4;
            hdr[8..12].copy_from_slice(&(real_body.len() as u32).to_le_bytes());
            hdr[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            let crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), payload);
            hdr[16..20].copy_from_slice(&crc.to_le_bytes());
            frame.extend_from_slice(&hdr);
            frame.extend_from_slice(payload);
            frame
        };

        // (a) Stripping the 4-byte LZ4 EndMark (00 00 00 00) is rejected.
        let stripped = &valid_payload[..valid_payload.len() - 4];
        let err_stripped = decode_batch(&frame_with_lz4_payload(stripped), None, None).unwrap_err();
        assert!(matches!(err_stripped, ColumnError::InvalidFrame { .. }));

        // (b) Replacing EndMark with 1, 2, or 3 arbitrary garbage bytes is rejected.
        for suffix in [&b"a"[..], &b"ab"[..], &b"abc"[..]] {
            let mut replaced = stripped.to_vec();
            replaced.extend_from_slice(suffix);
            let err_rep = decode_batch(&frame_with_lz4_payload(&replaced), None, None).unwrap_err();
            assert!(matches!(err_rep, ColumnError::InvalidFrame { .. }));
        }

        // (c) Appending 1..=4 trailing bytes after a valid EndMark is rejected.
        for extra in 1..=4usize {
            let mut trailing = valid_payload.clone();
            trailing.extend(std::iter::repeat_n(0x55, extra));
            let err_trail =
                decode_batch(&frame_with_lz4_payload(&trailing), None, None).unwrap_err();
            assert!(matches!(
                err_trail,
                ColumnError::InvalidFrame {
                    reason: "trailing bytes after lz4 frame"
                }
            ));
        }
    }

    #[test]
    fn encode_const_large_utf8_and_list_under_tight_reservation() {
        let schema = Schema::try_new_arc(vec![
            Field::new("big_s", DataType::Utf8, false),
            Field::new("big_l", DataType::List(Box::new(DataType::I64)), false),
        ])
        .unwrap();
        let big_str = "z".repeat(16 * 1024);
        let big_list = ScalarValue::List {
            item_type: DataType::I64,
            values: (0..1024).map(ScalarValue::I64).collect(),
        };
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::try_new_const(ScalarValue::Utf8(big_str), 4).unwrap(),
                Column::try_new_const(big_list, 4).unwrap(),
            ],
            4,
        )
        .unwrap();

        // Tight reservation smaller than uncompressed body is rejected before write_body with MemoryLimit.
        let tight = GlobalPool::standalone(4096);
        let mut tight_res = tight.register("const_encode_tight", true);
        let mut out_err = Vec::new();
        let err = encode_batch(&batch, &mut out_err, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 0);
        assert!(out_err.is_empty());

        // Ample reservation encodes cleanly, restores size, and round-trips semantically.
        let ample = GlobalPool::standalone(1024 * 1024);
        let mut res = ample.register("const_encode_ample", true);
        res.try_grow(16).unwrap();
        let mut frame = Vec::new();
        encode_batch(&batch, &mut frame, Some(&mut res)).unwrap();
        assert_eq!(res.size(), 16);

        let decoded = decode_batch(&frame, Some(&schema), None).unwrap();
        assert!(batch.semantic_eq(&decoded));
    }

    #[test]
    fn dict_and_rle_huge_unused_or_bool_does_not_oom_or_bypass_pool() {
        // 1. 42-byte Dict(Null) with raw_len = 0, dict_len = 1_000_000_000 + fuzz_entry replay.
        let mut body_zero = Vec::new();
        write_body_header(&mut body_zero, 0, "a", &DataType::Null);
        body_zero.push(2); // encoding = Dict
        body_zero.extend_from_slice(&1_000_000_000u32.to_le_bytes()); // dict_len = 1e9
        body_zero.push(0); // values has_nulls = false (FlatData::Null writes 0 bytes)
        body_zero.push(0); // dict nulls = None
        let frame_zero = wrap_uncompressed_body(&body_zero);
        assert_eq!(frame_zero.len(), 42);

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("dict_null_0", true);
        let decoded = decode_batch(&frame_zero, None, Some(&mut res)).unwrap();
        assert_eq!(decoded.num_rows(), 0);
        assert_eq!(res.size(), decoded.memory_bytes());
        crate::fuzz_entry::column_frame(&frame_zero);

        // 2. 46-byte Dict(Null) with raw_len = 1, dict_len = 1_000_000_000.
        let mut body_one = Vec::new();
        write_body_header(&mut body_one, 1, "a", &DataType::Null);
        body_one.push(2); // encoding = Dict
        body_one.extend_from_slice(&1_000_000_000u32.to_le_bytes()); // dict_len = 1e9
        body_one.push(0); // values has_nulls = false
        body_one.extend_from_slice(&0u32.to_le_bytes()); // codes = [0]
        body_one.push(0); // dict nulls = None
        let frame_one = wrap_uncompressed_body(&body_one);
        assert_eq!(frame_one.len(), 46);

        let mut res_one = scope.register("dict_null_1", true);
        let decoded_one = decode_batch(&frame_one, None, Some(&mut res_one)).unwrap();
        assert_eq!(decoded_one.num_rows(), 1);
        assert_eq!(res_one.size(), decoded_one.memory_bytes());
        crate::fuzz_entry::column_frame(&frame_one);

        // 3. High-cardinality Dict(Bool) under a tight pool (32 KiB) takes fixed-width O(1) fast path.
        let dict_bool_vals =
            FlatColumn::try_new(FlatData::Bool(Bitmap::new_ones(80_000)), None).unwrap();
        let dict_bool_col =
            Column::Dict(DictColumn::try_new(dict_bool_vals, vec![0], None).unwrap());
        let schema_bool =
            Schema::try_new_arc(vec![Field::new("b", DataType::Bool, false)]).unwrap();
        let batch_bool = Batch::try_new_all(schema_bool, vec![dict_bool_col], 1).unwrap();
        let mut body_bool = Vec::new();
        write_body(&batch_bool, &mut body_bool).unwrap();
        let frame_bool = wrap_uncompressed_body(&body_bool);

        let tight_bool = GlobalPool::standalone(32 * 1024);
        let mut res_bool = tight_bool.register("dict_bool", true);
        let decoded_bool = decode_batch(&frame_bool, None, Some(&mut res_bool)).unwrap();
        assert!(batch_bool.semantic_eq(&decoded_bool));
        assert_eq!(res_bool.size(), decoded_bool.memory_bytes());

        // 3b. High-cardinality Dict(List(I64)) memo map respects tight pool (never bypasses Reservation).
        let n_entries = 256usize;
        let leaf_i64 =
            FlatColumn::try_new(FlatData::I64((0..n_entries as i64).collect()), None).unwrap();
        let list_offsets: Vec<u32> = (0..=n_entries as u32).collect();
        let list_fc = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::I64,
                offsets: list_offsets,
                values: Box::new(Column::Flat(leaf_i64)),
            },
            None,
        )
        .unwrap();
        let dict_list_col = Column::Dict(
            DictColumn::try_new(list_fc, (0..n_entries as u32).collect(), None).unwrap(),
        );
        let schema_dl = Schema::try_new_arc(vec![Field::new(
            "dl",
            DataType::List(Box::new(DataType::I64)),
            false,
        )])
        .unwrap();
        let batch_dl = Batch::try_new_all(schema_dl, vec![dict_list_col], n_entries).unwrap();
        let mut body_dl = Vec::new();
        write_body(&batch_dl, &mut body_dl).unwrap();
        let frame_dl = wrap_uncompressed_body(&body_dl);

        // Pool fits decoded batch + 512B, but NOT the 256-entry memo map (>= 6 KiB).
        let tight_dl = GlobalPool::standalone(batch_dl.memory_bytes() + 512);
        let mut res_dl_tight = tight_dl.register("dict_list_tight", true);
        let err_dl = decode_batch(&frame_dl, None, Some(&mut res_dl_tight)).unwrap_err();
        assert!(matches!(err_dl, ColumnError::MemoryLimit { .. }));
        assert_eq!(res_dl_tight.size(), 0);

        // Ample pool decodes and shrinks memo reservation back on Drop.
        let mut res_dl_ok = scope.register("dict_list_ok", true);
        let decoded_dl = decode_batch(&frame_dl, None, Some(&mut res_dl_ok)).unwrap();
        assert!(batch_dl.semantic_eq(&decoded_dl));
        assert_eq!(res_dl_ok.size(), decoded_dl.memory_bytes());

        // 4. Dict(List(Const(List(I64)))) quadratic CPU guard via const_cache.
        let inner_list_scalar = ScalarValue::List {
            item_type: DataType::I64,
            values: (0..256).map(ScalarValue::I64).collect(),
        };
        let n_dict = 1_000usize;
        let inner_const = Column::try_new_const(inner_list_scalar, n_dict).unwrap();
        let outer_list_fc = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::List(Box::new(DataType::I64)),
                offsets: (0..=n_dict as u32).collect(),
                values: Box::new(inner_const),
            },
            None,
        )
        .unwrap();
        let quad_col = Column::Dict(
            DictColumn::try_new(outer_list_fc, (0..n_dict as u32).collect(), None).unwrap(),
        );
        let quad_schema = Schema::try_new_arc(vec![Field::new(
            "q",
            DataType::List(Box::new(DataType::List(Box::new(DataType::I64)))),
            false,
        )])
        .unwrap();
        let quad_batch = Batch::try_new_all(quad_schema, vec![quad_col], n_dict).unwrap();
        let mut quad_frame = Vec::new();
        encode_batch(&quad_batch, &mut quad_frame, None).unwrap();

        let ample = GlobalPool::standalone(16 * 1024 * 1024);
        let mut res_quad_ok = ample.register("quad_ok", true);
        let quad_decoded = decode_batch(&quad_frame, None, Some(&mut res_quad_ok)).unwrap();
        assert!(quad_batch.semantic_eq(&quad_decoded));
        assert_eq!(res_quad_ok.size(), quad_decoded.memory_bytes());

        // Under smaller max_decoded (< ~2 MiB materialized), rejected quickly via const_cache.
        let mut res_quad_err = ample.register("quad_err", true);
        let err_quad =
            decode_batch_with_limit(&quad_frame, None, Some(&mut res_quad_err), 1_000_000)
                .unwrap_err();
        assert!(matches!(
            err_quad,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_quad_err.size(), 0);

        // 5. 8-level alternating Rle(List(Dict(List(...)))) — both valid and explosive.
        let build_alternating = |fanout: usize| -> Batch {
            let mut col =
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![7i64; fanout]), None).unwrap());
            let mut dt = DataType::I64;
            for level in 0..MAX_NESTING_DEPTH {
                let list_fc = FlatColumn::try_new(
                    FlatData::List {
                        item_type: dt.clone(),
                        offsets: vec![0, fanout as u32],
                        values: Box::new(col),
                    },
                    None,
                )
                .unwrap();
                col = if level % 2 == 0 {
                    Column::Dict(DictColumn::try_new(list_fc, vec![0u32; fanout], None).unwrap())
                } else {
                    Column::Rle(RleColumn::try_new(list_fc, vec![fanout as u32]).unwrap())
                };
                dt = DataType::List(Box::new(dt));
            }
            let schema = Schema::try_new_arc(vec![Field::new("alt", dt, false)]).unwrap();
            Batch::try_new_all(schema, vec![col], fanout).unwrap()
        };

        let alt_small = build_alternating(2);
        let mut alt_small_frame = Vec::new();
        encode_batch(&alt_small, &mut alt_small_frame, None).unwrap();
        let mut res_alt_ok = scope.register("alt_ok", true);
        let alt_small_dec = decode_batch(&alt_small_frame, None, Some(&mut res_alt_ok)).unwrap();
        assert!(alt_small.semantic_eq(&alt_small_dec));
        assert_eq!(res_alt_ok.size(), alt_small_dec.memory_bytes());

        let alt_bomb = build_alternating(16);
        let mut enc_alt_out = Vec::new();
        let err_alt_enc = encode_batch(&alt_bomb, &mut enc_alt_out, None).unwrap_err();
        assert!(matches!(
            err_alt_enc,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert!(enc_alt_out.is_empty());

        let mut alt_bomb_body = Vec::new();
        write_body(&alt_bomb, &mut alt_bomb_body).unwrap();
        let alt_bomb_frame = wrap_uncompressed_body(&alt_bomb_body);
        let mut res_alt_err = scope.register("alt_err", true);
        let err_alt_dec = decode_batch(&alt_bomb_frame, None, Some(&mut res_alt_err)).unwrap_err();
        assert!(matches!(
            err_alt_dec,
            ColumnError::InvalidFrame {
                reason: "decoded materialized size exceeds max_decoded"
            }
        ));
        assert_eq!(res_alt_err.size(), 0);
    }

    #[test]
    fn encode_batch_rejects_frames_exceeding_decode_limits() {
        let scope = GlobalPool::standalone(4 * 1024 * 1024);

        // 1. 8192 x 132 KiB Utf8 via Const, Dict, and Rle -> > 1 GiB materialized, rejected on encode.
        let rows = 8192usize;
        let payload_str = "a".repeat(132 * 1024);
        let utf8_schema =
            Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices([payload_str.as_str()]).unwrap()),
            None,
        )
        .unwrap();
        let rle_vals = dict_vals.clone();

        let bomb_batches = [
            Batch::try_new_all(
                Arc::clone(&utf8_schema),
                vec![Column::try_new_const(ScalarValue::Utf8(payload_str), rows).unwrap()],
                rows,
            )
            .unwrap(),
            Batch::try_new_all(
                Arc::clone(&utf8_schema),
                vec![Column::Dict(
                    DictColumn::try_new(dict_vals, vec![0u32; rows], None).unwrap(),
                )],
                rows,
            )
            .unwrap(),
            Batch::try_new_all(
                Arc::clone(&utf8_schema),
                vec![Column::Rle(
                    RleColumn::try_new(rle_vals, vec![rows as u32]).unwrap(),
                )],
                rows,
            )
            .unwrap(),
        ];

        for (idx, bomb) in bomb_batches.iter().enumerate() {
            let mut out = vec![0xAB];
            let mut res = scope.register("enc_mat_bomb", true);
            res.try_grow(32).unwrap();
            let err = encode_batch(bomb, &mut out, Some(&mut res)).unwrap_err();
            assert!(
                matches!(
                    err,
                    ColumnError::InvalidFrame {
                        reason: "decoded materialized size exceeds max_decoded"
                    }
                ),
                "case {idx}: unexpected err {err:?}"
            );
            assert_eq!(out, vec![0xAB]);
            assert_eq!(res.size(), 32);
        }

        // 2. raw_len > max_decoded rejected before compaction; compacted selection within limit accepted.
        let i64_schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let big_const = Batch::try_new_all(
            Arc::clone(&i64_schema),
            vec![Column::try_new_const(ScalarValue::I64(1), 1000).unwrap()],
            1000,
        )
        .unwrap();
        let mut out = vec![0x11];
        let mut res = scope.register("raw_len_limit", true);
        res.try_grow(16).unwrap();
        let err = encode_batch_with_limit(&big_const, &mut out, Some(&mut res), 500).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::InvalidFrame {
                reason: "batch raw_len exceeds max_decoded"
            }
        ));
        assert_eq!(out, vec![0x11]);
        assert_eq!(res.size(), 16);

        let sel_expand = Batch::try_new(
            Arc::clone(&i64_schema),
            vec![Column::try_new_const(ScalarValue::I64(1), 1).unwrap()],
            1,
            Selection::Indices(vec![0u32; 1000]),
        )
        .unwrap();
        let err_sel =
            encode_batch_with_limit(&sel_expand, &mut out, Some(&mut res), 500).unwrap_err();
        assert!(matches!(
            err_sel,
            ColumnError::InvalidFrame {
                reason: "batch raw_len exceeds max_decoded"
            }
        ));
        assert_eq!(out, vec![0x11]);
        assert_eq!(res.size(), 16);

        let sel_shrink = Batch::try_new(
            Arc::clone(&i64_schema),
            vec![Column::try_new_const(ScalarValue::I64(7), 1000).unwrap()],
            1000,
            Selection::Indices(vec![0]),
        )
        .unwrap();
        let mut shrink_frame = Vec::new();
        encode_batch_with_limit(&sel_shrink, &mut shrink_frame, Some(&mut res), 500).unwrap();
        assert_eq!(res.size(), 16);
        let shrink_dec = decode_batch_with_limit(&shrink_frame, None, None, 500).unwrap();
        assert_eq!(shrink_dec.num_rows(), 1);
        assert!(sel_shrink.semantic_eq(&shrink_dec));

        // 3. logical_elements limits on List, Dict, and Rle.
        let list_null_schema = Schema::try_new_arc(vec![Field::new(
            "ln",
            DataType::List(Box::new(DataType::Null)),
            true,
        )])
        .unwrap();
        let list_logical_bomb = Batch::try_new_all(
            list_null_schema,
            vec![Column::Flat(
                FlatColumn::try_new(
                    FlatData::List {
                        item_type: DataType::Null,
                        offsets: vec![0, 300],
                        values: Box::new(
                            Column::try_new_const(ScalarValue::Null(DataType::Null), 300).unwrap(),
                        ),
                    },
                    None,
                )
                .unwrap(),
            )],
            1,
        )
        .unwrap();
        let err_ll = encode_batch_with_limit(&list_logical_bomb, &mut out, None, 256).unwrap_err();
        assert!(matches!(
            err_ll,
            ColumnError::InvalidFrame {
                reason: "list child length exceeds max_decoded"
            }
        ));

        let null_schema = Schema::try_new_arc(vec![Field::new("n", DataType::Null, true)]).unwrap();
        let dict_logical_bomb = Batch::try_new_all(
            null_schema,
            vec![Column::Dict(
                DictColumn::try_new(
                    FlatColumn::try_new(FlatData::Null(300), None).unwrap(),
                    vec![],
                    None,
                )
                .unwrap(),
            )],
            0,
        )
        .unwrap();
        let err_dl = encode_batch_with_limit(&dict_logical_bomb, &mut out, None, 256).unwrap_err();
        assert!(matches!(
            err_dl,
            ColumnError::InvalidFrame {
                reason: "dictionary length exceeds max_decoded"
            }
        ));

        let rle_null_schema =
            Schema::try_new_arc(vec![Field::new("a", DataType::Null, true)]).unwrap();
        let mut rle_run_ends: Vec<u32> = (1..=19u32).collect();
        rle_run_ends.push(200);
        let rle_logical_bomb = Batch::try_new_all(
            rle_null_schema,
            vec![Column::Rle(
                RleColumn::try_new(
                    FlatColumn::try_new(FlatData::Null(20), None).unwrap(),
                    rle_run_ends,
                )
                .unwrap(),
            )],
            200,
        )
        .unwrap();
        // wire_len (~101B) <= 210 and raw_len(200) <= 210, but raw_len(200) + run_count(20) = 220 > 210.
        let err_rl = encode_batch_with_limit(&rle_logical_bomb, &mut out, None, 210).unwrap_err();
        assert!(matches!(
            err_rl,
            ColumnError::InvalidFrame {
                reason: "rle run_count exceeds max_decoded"
            }
        ));

        // 4. Nested Const heap exceeding max_decoded even when len == 0 (wire size fits limit, heap does not).
        let big_const_list_scalar = ScalarValue::List {
            item_type: DataType::I64,
            values: (0..100).map(ScalarValue::I64).collect(),
        };
        let scalar_heap = big_const_list_scalar.heap_bytes();
        let nested_const_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::List(Box::new(DataType::I64)),
                    offsets: vec![0],
                    values: Box::new(
                        Column::try_new_const(big_const_list_scalar.clone(), 0).unwrap(),
                    ),
                },
                None,
            )
            .unwrap(),
        );
        let nested_zero_schema = Schema::try_new_arc(vec![Field::new(
            "nl",
            DataType::List(Box::new(DataType::List(Box::new(DataType::I64)))),
            false,
        )])
        .unwrap();
        let nested_zero_batch =
            Batch::try_new_all(nested_zero_schema, vec![nested_const_col], 0).unwrap();
        let wire_len = estimate_body_len(&nested_zero_batch, MAX_DECODED_BYTES).unwrap();
        let cap = u32::try_from(wire_len + 64).unwrap();
        assert!((cap as usize) < scalar_heap);

        let err_nested_zero =
            encode_batch_with_limit(&nested_zero_batch, &mut out, Some(&mut res), cap).unwrap_err();
        assert!(matches!(
            err_nested_zero,
            ColumnError::InvalidFrame {
                reason: "const scalar exceeds decoded size limit"
            }
        ));
        assert_eq!(out, vec![0x11]);
        assert_eq!(res.size(), 16);

        // Top-level Const at len == 0 with heap_bytes > max_decoded also rejected.
        let top_zero_schema = Schema::try_new_arc(vec![Field::new(
            "tl",
            DataType::List(Box::new(DataType::I64)),
            false,
        )])
        .unwrap();
        let top_zero_batch = Batch::try_new_all(
            top_zero_schema,
            vec![Column::try_new_const(big_const_list_scalar, 0).unwrap()],
            0,
        )
        .unwrap();
        let err_top_zero =
            encode_batch_with_limit(&top_zero_batch, &mut out, Some(&mut res), cap).unwrap_err();
        assert!(matches!(
            err_top_zero,
            ColumnError::InvalidFrame {
                reason: "const scalar exceeds decoded size limit"
            }
        ));
        assert_eq!(out, vec![0x11]);
        assert_eq!(res.size(), 16);
    }

    #[test]
    fn rle_run_count_exceeding_len_or_max_decoded_rejected_before_alloc() {
        let tiny_scope = GlobalPool::standalone(64);

        // 1. Rle(Null) with raw_len = 4 and run_count = 1_000_000 rejected before any allocation.
        let mut body_exceeds = Vec::new();
        write_body_header(&mut body_exceeds, 4, "r", &DataType::Null);
        body_exceeds.push(3); // Rle
        body_exceeds.extend_from_slice(&1_000_000u32.to_le_bytes()); // run_count > len
        let frame_exceeds = wrap_uncompressed_body(&body_exceeds);

        let mut res1 = tiny_scope.register("rle_exceeds_len", true);
        let err1 = decode_batch(&frame_exceeds, None, Some(&mut res1)).unwrap_err();
        assert!(matches!(
            err1,
            ColumnError::InvalidFrame {
                reason: "rle run_count exceeds column length"
            }
        ));
        assert_eq!(res1.size(), 0);

        // 2. Non-empty Rle with run_count = 0 rejected immediately.
        let mut body_zero = Vec::new();
        write_body_header(&mut body_zero, 4, "r", &DataType::I64);
        body_zero.push(3); // Rle
        body_zero.extend_from_slice(&0u32.to_le_bytes()); // run_count = 0 for len = 4
        let frame_zero = wrap_uncompressed_body(&body_zero);

        let mut res2 = tiny_scope.register("rle_zero_runs", true);
        let err2 = decode_batch(&frame_zero, None, Some(&mut res2)).unwrap_err();
        assert!(matches!(
            err2,
            ColumnError::InvalidFrame {
                reason: "non-empty rle column has zero runs"
            }
        ));
        assert_eq!(res2.size(), 0);

        // 3. Cumulative logical_elements (raw_len + rle run_count) exceeding max_decoded rejected before alloc.
        let mut body_multi = Vec::new();
        body_multi.extend_from_slice(&100u32.to_le_bytes()); // raw_len = 100
        body_multi.extend_from_slice(&2u32.to_le_bytes()); // col_count = 2
        for name in ["a", "b"] {
            body_multi.extend_from_slice(&(name.len() as u32).to_le_bytes());
            body_multi.extend_from_slice(name.as_bytes());
            body_multi.push(1);
            write_data_type(&mut body_multi, &DataType::Null);
        }
        // Column 0: Flat(Null) (2 bytes on wire; logical_elements remains 100).
        body_multi.push(0); // Flat
        body_multi.push(0); // has_nulls = false
        // Column 1: Rle(Null) with run_count = 60 <= len(100), but 100 + 60 = 160 > 150 — rejected before reading values/run_ends.
        body_multi.push(3); // Rle
        body_multi.extend_from_slice(&60u32.to_le_bytes());
        let frame_multi = wrap_uncompressed_body(&body_multi);
        assert!(body_multi.len() < 150);

        let scope_multi = GlobalPool::standalone(1024);
        let mut res3 = scope_multi.register("rle_max_decoded", true);
        let err3 = decode_batch_with_limit(&frame_multi, None, Some(&mut res3), 150).unwrap_err();
        assert!(matches!(
            err3,
            ColumnError::InvalidFrame {
                reason: "rle run_count exceeds max_decoded"
            }
        ));
        assert_eq!(res3.size(), 0);
    }

    #[test]
    fn const_decode_peak_reservation_and_utf8_zero_clone() {
        // 1. Const(Utf8) moves bytes without 2x clone: fits 1x pool (bytes + 8B offsets).
        let payload_len = 8192usize;
        let utf8_schema =
            Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let utf8_batch = Batch::try_new_all(
            Arc::clone(&utf8_schema),
            vec![Column::try_new_const(ScalarValue::Utf8("a".repeat(payload_len)), 4).unwrap()],
            4,
        )
        .unwrap();
        let mut utf8_body = Vec::new();
        write_body(&utf8_batch, &mut utf8_body).unwrap();
        let utf8_frame = wrap_uncompressed_body(&utf8_body);

        let exact_utf8_peak = payload_len + 8; // 2 u32 offsets + payload bytes
        let scope_utf8 = GlobalPool::standalone(exact_utf8_peak);
        let mut res_utf8 = scope_utf8.register("const_utf8_1x", true);
        let dec_utf8 = decode_batch(&utf8_frame, Some(&utf8_schema), Some(&mut res_utf8)).unwrap();
        assert!(utf8_batch.semantic_eq(&dec_utf8));
        assert_eq!(res_utf8.size(), payload_len);
        assert_eq!(res_utf8.size(), dec_utf8.memory_bytes());

        let under_utf8 = GlobalPool::standalone(exact_utf8_peak - 1);
        let mut res_utf8_err = under_utf8.register("const_utf8_under", true);
        let err_utf8 = decode_batch(&utf8_frame, None, Some(&mut res_utf8_err)).unwrap_err();
        assert!(matches!(err_utf8, ColumnError::MemoryLimit { .. }));
        assert_eq!(res_utf8_err.size(), 0);

        // 2. Const(Binary) moves bytes without 2x clone: fits 1x pool (bytes + 8B offsets).
        let bin_schema =
            Schema::try_new_arc(vec![Field::new("b", DataType::Binary, false)]).unwrap();
        let bin_batch = Batch::try_new_all(
            Arc::clone(&bin_schema),
            vec![Column::try_new_const(ScalarValue::Binary(vec![0x5A; payload_len]), 4).unwrap()],
            4,
        )
        .unwrap();
        let mut bin_body = Vec::new();
        write_body(&bin_batch, &mut bin_body).unwrap();
        let bin_frame = wrap_uncompressed_body(&bin_body);

        let scope_bin = GlobalPool::standalone(exact_utf8_peak);
        let mut res_bin = scope_bin.register("const_bin_1x", true);
        let dec_bin = decode_batch(&bin_frame, Some(&bin_schema), Some(&mut res_bin)).unwrap();
        assert!(bin_batch.semantic_eq(&dec_bin));
        assert_eq!(res_bin.size(), payload_len);
        assert_eq!(res_bin.size(), dec_bin.memory_bytes());

        // 3. Const(List(I64)) enforces true 2x peak (flat + scalar) and releases flat scratch between columns.
        let list_col = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: (0..128).map(ScalarValue::I64).collect(),
            },
            4,
        )
        .unwrap();
        let flat_bytes = 2 * 4 + 128 * 8; // offsets(8) + i64 values(1024)
        let scalar_bytes = list_col.memory_bytes();

        let one_schema = Schema::try_new_arc(vec![Field::new(
            "l0",
            DataType::List(Box::new(DataType::I64)),
            false,
        )])
        .unwrap();
        let one_batch =
            Batch::try_new_all(Arc::clone(&one_schema), vec![list_col.clone()], 4).unwrap();
        let mut one_body = Vec::new();
        write_body(&one_batch, &mut one_body).unwrap();
        let one_frame = wrap_uncompressed_body(&one_body);

        let under_one = GlobalPool::standalone(flat_bytes + scalar_bytes - 1);
        let mut res_one_err = under_one.register("const_list_under", true);
        let err_one = decode_batch(&one_frame, None, Some(&mut res_one_err)).unwrap_err();
        assert!(matches!(err_one, ColumnError::MemoryLimit { .. }));
        assert_eq!(res_one_err.size(), 0);

        let exact_one = GlobalPool::standalone(flat_bytes + scalar_bytes);
        let mut res_one_ok = exact_one.register("const_list_exact", true);
        let dec_one = decode_batch(&one_frame, Some(&one_schema), Some(&mut res_one_ok)).unwrap();
        assert!(one_batch.semantic_eq(&dec_one));
        assert_eq!(res_one_ok.size(), scalar_bytes);

        // Two Const(List(I64)) columns: col 0's flat scratch is released before col 1 decodes,
        // so peak is 2 * scalar_bytes + flat_bytes (strictly less than 2 * (scalar_bytes + flat_bytes)).
        let two_schema = Schema::try_new_arc(vec![
            Field::new("l0", DataType::List(Box::new(DataType::I64)), false),
            Field::new("l1", DataType::List(Box::new(DataType::I64)), false),
        ])
        .unwrap();
        let two_batch =
            Batch::try_new_all(Arc::clone(&two_schema), vec![list_col.clone(), list_col], 4)
                .unwrap();
        let mut two_body = Vec::new();
        write_body(&two_batch, &mut two_body).unwrap();
        let two_frame = wrap_uncompressed_body(&two_body);

        let two_peak = 2 * scalar_bytes + flat_bytes;
        let exact_two = GlobalPool::standalone(two_peak);
        let mut res_two_ok = exact_two.register("const_list_two_ok", true);
        let dec_two = decode_batch(&two_frame, Some(&two_schema), Some(&mut res_two_ok)).unwrap();
        assert!(two_batch.semantic_eq(&dec_two));
        assert_eq!(res_two_ok.size(), 2 * scalar_bytes);
        assert_eq!(res_two_ok.size(), dec_two.memory_bytes());

        let under_two = GlobalPool::standalone(two_peak - 1);
        let mut res_two_err = under_two.register("const_list_two_err", true);
        let err_two = decode_batch(&two_frame, None, Some(&mut res_two_err)).unwrap_err();
        assert!(matches!(err_two, ColumnError::MemoryLimit { .. }));
        assert_eq!(res_two_err.size(), 0);
    }
}
